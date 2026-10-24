//! The L0 host API on the server side (decisions #166): subscriber connections, topic
//! publication, and the chrome claims that let a host UI draw the sidebar / status bar
//! instead of the TUI.
//!
//! Wire types and the client half live in `copad-mux-proto`; this module owns the delivery
//! contract the codex rounds pinned down:
//!
//! * **Never block the single-writer loop.** Each subscriber has an [`Outbox`] drained by its
//!   own writer thread. The loop only ever appends.
//! * **Reliable messages are delivered or the connection dies.** Welcome, snapshot, replies,
//!   pongs and events go through a byte-capped FIFO; an append that would exceed the cap
//!   disconnects the subscriber instead of dropping a reply whose mutation already ran.
//! * **State coalesces per topic.** Each topic has one slot holding only its latest value, so
//!   a slow reader skips intermediate values (each is complete) without growing memory.
//! * **Fairness.** The writer takes a bounded batch of reliable lines, then every slot, on each
//!   pass — a busy reply stream cannot starve the topics or vice versa.
//! * **A stall is a disconnect.** A line that cannot be written within [`WRITE_DEADLINE`]
//!   kills the connection, and the writer tells the loop at once (`SubGone`) so a host that
//!   stopped reading cannot keep the TUI chrome hidden.

use copad_mux_proto::{self as l0, Feature, PubMsg, TopicValue};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Reliable bytes a subscriber may have queued before it is cut off. Above the largest
/// snapshot (five topics, each capped at [`TOPIC_CAP`]), so a healthy subscribe always fits.
const RELIABLE_CAP: usize = 8 << 20;

/// A single topic value larger than this is published as `unavailable` instead.
pub const TOPIC_CAP: usize = 1 << 20;

/// Reliable lines per writer pass before the slots get their turn.
const RELIABLE_BATCH: usize = 16;

/// How long one line may take to write before the subscriber is declared stalled.
const WRITE_DEADLINE: Duration = Duration::from_secs(10);

/// Minimum spacing between two publication passes.
pub const PUBLISH_INTERVAL: Duration = Duration::from_millis(100);

/// What a subscriber's writer thread sends: reliable lines first in bounded batches, then
/// the latest value of every dirty topic.
pub struct Outbox {
    inner: Mutex<Inner>,
    cv: Condvar,
}

#[derive(Default)]
struct Inner {
    reliable: VecDeque<String>,
    bytes: usize,
    slots: BTreeMap<&'static str, String>,
    dead: bool,
}

impl Outbox {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            cv: Condvar::new(),
        })
    }

    /// Queue a line that must arrive. `false` = the subscriber is over its byte budget (or
    /// already dead) and must be disconnected.
    pub fn push(&self, line: String) -> bool {
        let Ok(mut g) = self.inner.lock() else {
            return false;
        };
        if g.dead || g.bytes + line.len() > RELIABLE_CAP {
            return false;
        }
        g.bytes += line.len();
        g.reliable.push_back(line);
        self.cv.notify_one();
        true
    }

    /// Replace `topic`'s pending value.
    pub fn slot(&self, topic: &'static str, line: String) {
        if let Ok(mut g) = self.inner.lock()
            && !g.dead
        {
            g.slots.insert(topic, line);
            self.cv.notify_one();
        }
    }

    /// Stop the writer (it exits once it wakes).
    pub fn kill(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.dead = true;
            g.reliable.clear();
            g.slots.clear();
            g.bytes = 0;
        }
        self.cv.notify_all();
    }

    /// The next batch to write, blocking until there is one. `None` = dead. Data is MOVED
    /// out so no socket write ever happens under the lock.
    fn next_batch(&self) -> Option<Vec<String>> {
        let mut g = self.inner.lock().ok()?;
        loop {
            if g.dead {
                return None;
            }
            if !g.reliable.is_empty() || !g.slots.is_empty() {
                break;
            }
            g = self.cv.wait(g).ok()?;
        }
        let mut out = Vec::new();
        for _ in 0..RELIABLE_BATCH {
            let Some(line) = g.reliable.pop_front() else {
                break;
            };
            g.bytes -= line.len();
            out.push(line);
        }
        out.extend(std::mem::take(&mut g.slots).into_values());
        Some(out)
    }
}

/// Drain `outbox` to `stream` until it dies or a write fails/stalls; then make sure the main
/// loop hears about it ([`crate::server`]'s `SubGone`) and the reader unblocks.
pub fn writer_loop<F: Fn()>(outbox: Arc<Outbox>, mut stream: UnixStream, gone: F) {
    let _ = stream.set_write_timeout(Some(WRITE_DEADLINE));
    'outer: while let Some(batch) = outbox.next_batch() {
        for mut line in batch {
            line.push('\n');
            let started = Instant::now();
            let ok = stream.write_all(line.as_bytes()).is_ok() && stream.flush().is_ok();
            if !ok || started.elapsed() > WRITE_DEADLINE {
                break 'outer;
            }
        }
    }
    outbox.kill();
    let _ = stream.shutdown(std::net::Shutdown::Both);
    gone();
}

fn line(msg: &PubMsg) -> String {
    serde_json::to_string(msg).unwrap_or_else(|_| "{\"t\":\"error\"}".into())
}

/// One live subscriber.
pub struct Sub {
    pub id: u64,
    pub outbox: Arc<Outbox>,
    pub conn: UnixStream,
    pub last_seen: Instant,
}

/// A render attachment as the host API needs to see it.
pub struct AttachView {
    pub id: u64,
    pub tag: Option<String>,
    pub cols: u16,
    pub rows: u16,
}

/// Every topic's freshly computed value, serialized.
pub struct TopicValues(pub Vec<(&'static str, String)>);

/// The host API's state on the main loop.
#[derive(Default)]
pub struct Hub {
    pub subs: Vec<Sub>,
    /// attachment id → (owning subscriber, claimed features). One owner per attachment.
    claims: HashMap<u64, (u64, BTreeSet<Feature>)>,
    rev: u64,
    /// Last published (serialized data, rev) per topic.
    last: BTreeMap<&'static str, (String, u64)>,
    last_pub: Option<Instant>,
    /// Highest notification id already announced as a `notify` event; `None` until the first
    /// publication pass, which only records the high-water mark.
    notif_seen: Option<u64>,
}

/// A reason a claim was refused, sent back as a protocol `error`.
pub type Refusal = (&'static str, String);

impl Hub {
    pub fn is_empty(&self) -> bool {
        self.subs.is_empty()
    }

    /// Whether a publication pass is due.
    pub fn due(&self) -> bool {
        !self.subs.is_empty()
            && self
                .last_pub
                .is_none_or(|t| t.elapsed() >= PUBLISH_INTERVAL)
    }

    /// Register a new subscriber: welcome + a snapshot of `values`, queued BEFORE it can
    /// receive any slot (it is not in `subs` until after), so the snapshot is always first.
    pub fn add(&mut self, sub: Sub, values: TopicValues, version: &str) {
        // Values that changed since the last pass are news to the EXISTING subscribers too:
        // folding them here without forwarding would let the next pass see them as unchanged.
        for (name, msg) in self.fold(values) {
            for s in &self.subs {
                s.outbox.slot(name, msg.clone());
            }
        }
        let welcome = PubMsg::Welcome {
            proto: l0::PROTO,
            incarnation: crate::term::incarnation().to_string(),
            version: version.to_string(),
        };
        // An oversized topic is left out of the snapshot and announced as unavailable right
        // after it — the cap must hold on (re)subscribe exactly as it does on publication.
        let mut unavailable = Vec::new();
        let topics = self
            .last
            .iter()
            .filter_map(|(name, (data, rev))| {
                if data.len() > TOPIC_CAP {
                    unavailable.push(PubMsg::Unavailable {
                        topic: name.to_string(),
                        rev: *rev,
                        reason: format!("{} bytes exceeds {TOPIC_CAP}", data.len()),
                    });
                    return None;
                }
                let data = serde_json::from_str(data).ok()?;
                Some((name.to_string(), TopicValue { rev: *rev, data }))
            })
            .collect();
        let snap = PubMsg::Snapshot {
            rev: self.rev,
            topics,
        };
        let queued = std::iter::once(&welcome)
            .chain(std::iter::once(&snap))
            .chain(unavailable.iter())
            .all(|m| sub.outbox.push(line(m)));
        if queued {
            self.subs.push(sub);
        } else {
            sub.outbox.kill();
            let _ = sub.conn.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Store new values, bumping `rev` for each that changed; returns the changed topics as
    /// ready-to-send lines (`state`, or `unavailable` for an oversized value).
    fn fold(&mut self, values: TopicValues) -> Vec<(&'static str, String)> {
        let mut changed = Vec::new();
        for (name, data) in values.0 {
            if self.last.get(name).is_some_and(|(d, _)| *d == data) {
                continue;
            }
            self.rev += 1;
            let rev = self.rev;
            let msg = if data.len() > TOPIC_CAP {
                PubMsg::Unavailable {
                    topic: name.into(),
                    rev,
                    reason: format!("{} bytes exceeds {TOPIC_CAP}", data.len()),
                }
            } else {
                PubMsg::State {
                    topic: name.into(),
                    rev,
                    data: serde_json::from_str(&data).unwrap_or_default(),
                }
            };
            changed.push((name, line(&msg)));
            // An oversized value is remembered as itself (so it is not re-announced every
            // pass) but never sent; the snapshot path skips it too because its JSON is
            // re-parsed only when under the cap.
            self.last.insert(name, (data, rev));
        }
        changed
    }

    /// One publication pass: changed topics into every subscriber's slots, then events.
    /// Returns the ids of subscribers that blew their budget (caller disconnects them).
    pub fn publish(
        &mut self,
        values: TopicValues,
        flashes: Vec<l0::Flash>,
        notifs: &l0::Notifications,
    ) -> Vec<u64> {
        self.last_pub = Some(Instant::now());
        for (name, msg) in self.fold(values) {
            for s in &self.subs {
                s.outbox.slot(name, msg.clone());
            }
        }
        let mut events: Vec<PubMsg> = flashes
            .into_iter()
            .map(|f| PubMsg::Event {
                kind: "flash".into(),
                data: serde_json::to_value(f).unwrap_or_default(),
            })
            .collect();
        // Newest-first list: announce every id above what was already announced, oldest
        // first. The first pass after start only records the high-water mark — a subscriber
        // connecting later must not get a toast for every notification in the log.
        let newest = notifs.items.first().map_or(0, |n| n.id);
        if let Some(seen) = self.notif_seen {
            for n in notifs.items.iter().rev().filter(|n| n.id > seen) {
                events.push(PubMsg::Event {
                    kind: "notify".into(),
                    data: serde_json::to_value(n).unwrap_or_default(),
                });
            }
        }
        self.notif_seen = Some(self.notif_seen.unwrap_or(0).max(newest));
        let mut over = Vec::new();
        for e in &events {
            let l = line(e);
            for s in &self.subs {
                if !s.outbox.push(l.clone()) {
                    over.push(s.id);
                }
            }
        }
        over
    }

    /// Queue a reliable message to one subscriber. `false` = it must be disconnected.
    pub fn send(&self, sub: u64, msg: &PubMsg) -> bool {
        self.subs
            .iter()
            .find(|s| s.id == sub)
            .is_none_or(|s| s.outbox.push(line(msg)))
    }

    /// Note activity from `sub` (any line keeps its claims alive).
    pub fn touch(&mut self, sub: u64) {
        if let Some(s) = self.subs.iter_mut().find(|s| s.id == sub) {
            s.last_seen = Instant::now();
        }
    }

    /// Drop a subscriber and every claim it held. Returns whether claims changed.
    pub fn remove(&mut self, sub: u64) -> bool {
        if let Some(pos) = self.subs.iter().position(|s| s.id == sub) {
            let s = self.subs.remove(pos);
            s.outbox.kill();
            let _ = s.conn.shutdown(std::net::Shutdown::Both);
        }
        let before = self.claims.len();
        self.claims.retain(|_, (owner, _)| *owner != sub);
        before != self.claims.len()
    }

    /// Claim `attachment` for `features` on behalf of `sub`.
    pub fn claim(
        &mut self,
        sub: u64,
        attachment: u64,
        incarnation: &str,
        features: Vec<Feature>,
        attached: &[AttachView],
    ) -> Result<(), Refusal> {
        if incarnation != crate::term::incarnation() {
            return Err((
                "stale",
                "the server restarted; resubscribe and claim again".into(),
            ));
        }
        let Some(a) = attached.iter().find(|a| a.id == attachment) else {
            return Err(("not_found", format!("no attachment {attachment}")));
        };
        if a.tag.as_deref().is_none_or(str::is_empty) {
            return Err((
                "refused",
                format!("attachment {attachment} has no binding tag"),
            ));
        }
        if let Some((owner, _)) = self.claims.get(&attachment)
            && *owner != sub
        {
            return Err((
                "refused",
                format!("attachment {attachment} is claimed by another host"),
            ));
        }
        self.claims
            .insert(attachment, (sub, features.into_iter().collect()));
        Ok(())
    }

    /// Release `attachment` if `sub` owns it. Returns whether claims changed.
    pub fn release(&mut self, sub: u64, attachment: u64) -> bool {
        if self.claims.get(&attachment).is_some_and(|(o, _)| *o == sub) {
            self.claims.remove(&attachment);
            return true;
        }
        false
    }

    /// Drop claims on attachments that are no longer attached (a detach, a disconnect, a
    /// pruned dead client — whichever path removed it).
    pub fn retain_attachments(&mut self, attached: &[AttachView]) {
        self.claims
            .retain(|id, _| attached.iter().any(|a| a.id == *id));
    }

    /// Drop the claims of owners silent for longer than the expiry. Returns whether any went.
    pub fn expire(&mut self) -> bool {
        let limit = Duration::from_secs(l0::CLAIM_EXPIRY_SECS);
        let silent: BTreeSet<u64> = self
            .subs
            .iter()
            .filter(|s| s.last_seen.elapsed() > limit)
            .map(|s| s.id)
            .collect();
        let before = self.claims.len();
        self.claims.retain(|_, (owner, _)| !silent.contains(owner));
        before != self.claims.len()
    }

    /// The shared-frame rule: a feature is hidden iff at least one render client is attached
    /// and EVERY one of them is claimed for it.
    pub fn hidden(&self, attached: &[AttachView]) -> BTreeSet<Feature> {
        if attached.is_empty() {
            return BTreeSet::new();
        }
        [Feature::Sidebar, Feature::Status, Feature::Overlays]
            .into_iter()
            .filter(|f| {
                attached
                    .iter()
                    .all(|a| self.claims.get(&a.id).is_some_and(|(_, fs)| fs.contains(f)))
            })
            .collect()
    }

    /// The subscriber that claims `attachment` for `feature`, if any.
    pub fn host_of(&self, attachment: u64, feature: Feature) -> Option<u64> {
        self.claims
            .get(&attachment)
            .filter(|(_, fs)| fs.contains(&feature))
            .map(|(o, _)| *o)
    }

    /// The `clients` topic value.
    pub fn clients_topic(&self, attached: &[AttachView]) -> l0::Clients {
        l0::Clients {
            clients: attached
                .iter()
                .map(|a| l0::ClientInfo {
                    id: a.id,
                    tag: a.tag.clone().unwrap_or_default(),
                    cols: a.cols,
                    rows: a.rows,
                    claimed: self
                        .claims
                        .get(&a.id)
                        .map(|(_, fs)| fs.iter().copied().collect())
                        .unwrap_or_default(),
                })
                .collect(),
            hidden: self.hidden(attached).into_iter().collect(),
        }
    }
}

/// Serialize a topic value; the publication compares these strings.
pub fn ser<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".into())
}

/// Spawn the writer for a new subscriber; `tx_gone` is called once when it ends.
pub fn spawn_writer(outbox: Arc<Outbox>, stream: UnixStream, tx_gone: impl Fn() + Send + 'static) {
    std::thread::spawn(move || writer_loop(outbox, stream, tx_gone));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    fn view(id: u64, tag: Option<&str>) -> AttachView {
        AttachView {
            id,
            tag: tag.map(str::to_string),
            cols: 80,
            rows: 24,
        }
    }

    fn sub_pair(id: u64) -> (Sub, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        (
            Sub {
                id,
                outbox: Outbox::new(),
                conn: a,
                last_seen: Instant::now(),
            },
            b,
        )
    }

    fn vals(tree: &str) -> TopicValues {
        TopicValues(vec![("tree", tree.to_string())])
    }

    #[test]
    fn reliable_overflow_reports_instead_of_dropping() {
        let o = Outbox::new();
        assert!(o.push("x".repeat(RELIABLE_CAP - 1)));
        assert!(!o.push("yy".into()), "over budget must be refused");
        o.kill();
        assert!(!o.push("z".into()), "a dead outbox refuses");
    }

    #[test]
    fn slots_keep_only_the_latest_value_and_batches_interleave() {
        let o = Outbox::new();
        for i in 0..40 {
            assert!(o.push(format!("r{i}")));
        }
        o.slot("tree", "t1".into());
        o.slot("tree", "t2".into());
        let b = o.next_batch().unwrap();
        // A bounded reliable batch, then the slots — the topic is not starved by 40 replies.
        assert_eq!(b.len(), RELIABLE_BATCH + 1);
        assert_eq!(b.last().unwrap(), "t2");
        assert_eq!(o.next_batch().unwrap().len(), RELIABLE_BATCH);
    }

    #[test]
    fn a_new_subscriber_gets_welcome_then_snapshot_first() {
        let mut hub = Hub::default();
        let (s, peer) = sub_pair(1);
        let ob = s.outbox.clone();
        let conn = s.conn.try_clone().unwrap();
        hub.add(s, vals(r#"{"sessions":[],"active":"a"}"#), "9");
        spawn_writer(ob, conn, || {});
        let mut r = BufReader::new(peer);
        let mut l = String::new();
        r.read_line(&mut l).unwrap();
        assert!(l.contains(r#""t":"welcome""#), "{l}");
        l.clear();
        r.read_line(&mut l).unwrap();
        let snap: PubMsg = serde_json::from_str(&l).unwrap();
        let PubMsg::Snapshot { topics, .. } = snap else {
            panic!("{l}")
        };
        assert_eq!(topics["tree"].data["active"], "a");
    }

    #[test]
    fn publish_sends_only_changed_topics_with_fresh_revisions() {
        let mut hub = Hub::default();
        let (s, _peer) = sub_pair(1);
        let ob = s.outbox.clone();
        hub.add(s, vals("{\"a\":1}"), "9");
        let _ = ob.next_batch(); // welcome + snapshot
        let n = l0::Notifications::default();
        hub.publish(vals("{\"a\":1}"), vec![], &n);
        assert!(
            ob.inner.lock().unwrap().slots.is_empty(),
            "unchanged => silent"
        );
        hub.publish(vals("{\"a\":2}"), vec![], &n);
        let b = ob.next_batch().unwrap();
        let msg: PubMsg = serde_json::from_str(&b[0]).unwrap();
        assert!(matches!(msg, PubMsg::State { rev: 2, .. }), "{msg:?}");
    }

    #[test]
    fn a_change_folded_by_a_new_subscriber_still_reaches_the_old_ones() {
        let mut hub = Hub::default();
        let (a, _pa) = sub_pair(1);
        let oa = a.outbox.clone();
        hub.add(a, vals("{\"v\":1}"), "9");
        let _ = oa.next_batch();
        let (b, _pb) = sub_pair(2);
        hub.add(b, vals("{\"v\":2}"), "9");
        let got = oa.next_batch().unwrap();
        assert!(got[0].contains(r#""v":2"#), "{got:?}");
    }

    #[test]
    fn a_snapshot_respects_the_topic_cap() {
        let mut hub = Hub::default();
        let big = format!("{{\"x\":\"{}\"}}", "a".repeat(TOPIC_CAP));
        let (s, _p) = sub_pair(1);
        let ob = s.outbox.clone();
        hub.add(s, vals(&big), "9");
        let b = ob.next_batch().unwrap();
        assert_eq!(b.len(), 3, "welcome, snapshot, unavailable");
        assert!(
            b[1].len() < 1000,
            "the oversized topic is not in the snapshot"
        );
        assert!(b[2].contains(r#""t":"unavailable""#));
    }

    #[test]
    fn an_oversized_topic_is_unavailable_not_truncated() {
        let mut hub = Hub::default();
        let (s, _peer) = sub_pair(1);
        let ob = s.outbox.clone();
        hub.add(s, vals("{}"), "9");
        let _ = ob.next_batch();
        let big = format!("{{\"x\":\"{}\"}}", "a".repeat(TOPIC_CAP));
        hub.publish(vals(&big), vec![], &l0::Notifications::default());
        let b = ob.next_batch().unwrap();
        assert!(b[0].contains(r#""t":"unavailable""#), "{}", &b[0][..80]);
    }

    #[test]
    fn notify_events_skip_the_backlog_and_announce_new_ids_oldest_first() {
        let mut hub = Hub::default();
        let (s, _peer) = sub_pair(1);
        let ob = s.outbox.clone();
        hub.add(s, vals("{}"), "9");
        let _ = ob.next_batch();
        let mk = |ids: &[u64]| l0::Notifications {
            items: ids
                .iter()
                .map(|&id| l0::Notification {
                    id,
                    ..Default::default()
                })
                .collect(),
            unread: 0,
        };
        hub.publish(vals("{}"), vec![], &mk(&[3, 2, 1]));
        assert!(
            ob.inner.lock().unwrap().reliable.is_empty(),
            "backlog is not toasted"
        );
        // And from an EMPTY log, the very first notification is announced.
        let mut fresh = Hub::default();
        let (s2, _p2) = sub_pair(2);
        let ob2 = s2.outbox.clone();
        fresh.add(s2, vals("{}"), "9");
        let _ = ob2.next_batch();
        fresh.publish(vals("{}"), vec![], &mk(&[]));
        fresh.publish(vals("{}"), vec![], &mk(&[1]));
        assert_eq!(
            ob2.next_batch().unwrap().len(),
            1,
            "id 1 after an empty log"
        );
        hub.publish(vals("{}"), vec![], &mk(&[5, 4, 3, 2, 1]));
        let b = ob.next_batch().unwrap();
        assert_eq!(b.len(), 2);
        assert!(
            b[0].contains(r#""id":4"#) && b[1].contains(r#""id":5"#),
            "{b:?}"
        );
    }

    #[test]
    fn hidden_needs_every_attachment_claimed() {
        let mut hub = Hub::default();
        let (s, _p) = sub_pair(7);
        hub.subs.push(s);
        let inc = crate::term::incarnation();
        let two = [view(1, Some("a")), view(2, None)];
        assert!(hub.hidden(&[]).is_empty(), "no clients => nothing hidden");
        hub.claim(7, 1, inc, vec![Feature::Sidebar], &two).unwrap();
        assert!(
            hub.hidden(&two).is_empty(),
            "client 2 still needs the TUI chrome"
        );
        assert_eq!(
            hub.claim(7, 2, inc, vec![Feature::Sidebar], &two)
                .unwrap_err()
                .0,
            "refused",
            "an untagged attachment cannot be claimed"
        );
        let one = [view(1, Some("a"))];
        assert!(hub.hidden(&one).contains(&Feature::Sidebar));
        assert!(!hub.hidden(&one).contains(&Feature::Status));
    }

    #[test]
    fn a_claim_has_one_owner_and_dies_with_it() {
        let mut hub = Hub::default();
        let (a, _pa) = sub_pair(1);
        let (b, _pb) = sub_pair(2);
        hub.subs.push(a);
        hub.subs.push(b);
        let inc = crate::term::incarnation();
        let one = [view(9, Some("t"))];
        hub.claim(1, 9, inc, vec![Feature::Status], &one).unwrap();
        assert_eq!(
            hub.claim(2, 9, inc, vec![Feature::Status], &one)
                .unwrap_err()
                .0,
            "refused"
        );
        assert_eq!(
            hub.claim(1, 9, "other", vec![Feature::Status], &one)
                .unwrap_err()
                .0,
            "stale"
        );
        assert!(hub.remove(1), "removing the owner releases the claim");
        assert!(hub.hidden(&one).is_empty());
        hub.claim(2, 9, inc, vec![Feature::Status], &one).unwrap();
    }

    #[test]
    fn silent_owners_lose_their_claims() {
        let mut hub = Hub::default();
        let (mut s, _p) = sub_pair(1);
        s.last_seen = Instant::now() - Duration::from_secs(l0::CLAIM_EXPIRY_SECS + 1);
        hub.subs.push(s);
        let one = [view(3, Some("t"))];
        hub.claim(
            1,
            3,
            crate::term::incarnation(),
            vec![Feature::Sidebar],
            &one,
        )
        .unwrap();
        assert!(hub.expire());
        assert!(hub.hidden(&one).is_empty());
    }

    #[test]
    fn a_stalled_reader_kills_the_writer_and_reports_gone() {
        let (a, b) = UnixStream::pair().unwrap();
        let o = Outbox::new();
        let (tx, rx) = std::sync::mpsc::channel();
        // Our write half is closed: the first write fails deterministically (a dropped PEER
        // can let the first write land in the kernel buffer on macOS, which flaked this).
        a.shutdown(std::net::Shutdown::Write).unwrap();
        drop(b);
        o.push("hello".into());
        spawn_writer(o.clone(), a, move || {
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(30))
            .expect("gone reported");
        assert!(!o.push("x".into()));
    }
}
