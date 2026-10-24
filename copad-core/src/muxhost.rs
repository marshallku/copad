//! A copad GUI as a comux HOST (decisions #169): keep an L0 subscription to a comux server,
//! find the comux clients running in this GUI's terminal panels, claim their sidebar, and
//! expose the lists for the GUI to draw natively.
//!
//! Shared by copad-linux (in process) and copad-macos (through copad-ffi), so the binding,
//! claim and reconnect rules exist once. The GUI tells it two things:
//!
//! * [`MuxHost::bind`] — a panel's binding tag: the tag a comux client in that tab reports
//!   (`copad_mux_proto::copad_tag(panel, gui_socket)`; a remote tab opened by copad passes the
//!   nonce it gave the remote client in `COPAD_MUX_HOST_TAG`).
//! * [`MuxHost::serve`] — which panels its windows are SHOWING right now. Only those are
//!   claimed (codex P2-C2): claiming a comux in a background tab would hide its TUI sidebar while
//!   no native sidebar is on screen for it.
//!
//! Claims are reconciled on EVERY topic change, not only `clients` (codex P2-C1): when a topic
//! the sidebar needs goes unavailable, the claim is released at once so the TUI sidebar comes
//! back, and taken again when the data returns.

use copad_mux_proto::link::{Endpoint, Link, LinkEvent};
use copad_mux_proto::{Applied, Feature, Model, PubMsg, SubMsg};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What a desktop host takes over: the sidebar, the status bar (every field it carries has a
/// native replacement in the window's comux status strip) and the overlays (`Ctrl-f`,
/// `Ctrl-b a`, prompts, confirms — decisions #170).
pub const FEATURES: [Feature; 3] = [Feature::Sidebar, Feature::Status, Feature::Overlays];

/// The claim state machine — pure, so its transitions are unit-tested without a server.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Claims {
    /// Attachments we hold a claim on.
    held: BTreeSet<u64>,
    /// The one HostReady in flight. Claims are serialized because the server's refusal
    /// (`PubMsg::Error`) carries no request id (codex P2-C3).
    pending: Option<u64>,
    /// Attachments another host owns; retried once `clients` shows them unclaimed.
    refused: BTreeSet<u64>,
    /// Claims sent per attachment: a refusal that is NOT about ownership (the claimed list
    /// stays empty) must not turn into a claim/refuse loop.
    attempts: BTreeMap<u64, u8>,
}

/// Claims per attachment before giving up on it (until it detaches).
const MAX_ATTEMPTS: u8 = 3;

/// A message the state machine wants sent.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaimOp {
    Ready(u64),
    Gone(u64),
}

impl Claims {
    /// Re-derive what we should hold from the model and the tags of the panels being served.
    pub fn reconcile(&mut self, model: &Model, served_tags: &BTreeSet<String>) -> Vec<ClaimOp> {
        let mut ops = Vec::new();
        // No `clients` value (unavailable, or not received yet) is NOT "everything detached":
        // we cannot tell, so release what we hold explicitly rather than forget it silently —
        // the server would otherwise keep the TUI sidebar hidden (codex P2 review C3).
        let Some(clients) = model.clients.as_ref().map(|c| c.clients.as_slice()) else {
            for a in std::mem::take(&mut self.held) {
                ops.push(ClaimOp::Gone(a));
            }
            if let Some(p) = self.pending.take() {
                ops.push(ClaimOp::Gone(p));
            }
            return ops;
        };
        let can = model.can_host(&FEATURES);
        let desired: BTreeSet<u64> = clients
            .iter()
            .filter(|c| can && !c.tag.is_empty() && served_tags.contains(&c.tag))
            .map(|c| c.id)
            .collect();
        // A pending claim the server confirmed (the `clients` topic lists it as claimed).
        if let Some(p) = self.pending
            && clients
                .iter()
                .any(|c| c.id == p && FEATURES.iter().all(|f| c.claimed.contains(f)))
        {
            self.pending = None;
            self.held.insert(p);
            // The budget is for refusals in a row, not for claims ever made.
            self.attempts.remove(&p);
        }
        // Gone from the list (detached) — nothing to release, the server already did.
        self.held.retain(|a| clients.iter().any(|c| c.id == *a));
        self.refused.retain(|a| clients.iter().any(|c| c.id == *a));
        self.attempts
            .retain(|a, _| clients.iter().any(|c| c.id == *a));
        if self
            .pending
            .is_some_and(|p| !clients.iter().any(|c| c.id == p))
        {
            self.pending = None;
        }
        // Held but no longer wanted (unserved, untagged, or the data went away): release.
        for a in self.held.clone() {
            if !desired.contains(&a) {
                self.held.remove(&a);
                ops.push(ClaimOp::Gone(a));
            }
        }
        // Refused earlier, and now nobody owns it: worth another try.
        self.refused
            .retain(|a| clients.iter().any(|c| c.id == *a && !c.claimed.is_empty()));
        if self.pending.is_none()
            && let Some(a) = desired.iter().find(|a| {
                !self.held.contains(a)
                    && !self.refused.contains(a)
                    && self.attempts.get(a).copied().unwrap_or(0) < MAX_ATTEMPTS
                    && clients.iter().any(|c| c.id == **a && c.claimed.is_empty())
            })
        {
            self.pending = Some(*a);
            *self.attempts.entry(*a).or_default() += 1;
            ops.push(ClaimOp::Ready(*a));
        }
        ops
    }

    /// The server refused the claim in flight.
    pub fn refused(&mut self) {
        if let Some(a) = self.pending.take() {
            self.refused.insert(a);
        }
    }

    pub fn held(&self) -> &BTreeSet<u64> {
        &self.held
    }

    /// Held, or claimed and not yet confirmed: the server accepts a claim at once but its
    /// `clients` topic can lag behind an overlay it already sent us (codex P3 C1).
    pub fn covers(&self, attachment: u64) -> bool {
        self.held.contains(&attachment) || self.pending == Some(attachment)
    }
}

/// Everything a GUI reads.
#[derive(Default)]
struct State {
    connected: bool,
    error: Option<String>,
    model: Model,
    /// panel id → binding tag.
    binds: BTreeMap<String, String>,
    served: BTreeSet<String>,
    claims: Claims,
    /// Overlays comux handed us for attachments we hold: (overlay id, attachment, spec).
    overlays: Vec<(u64, u64, copad_mux_proto::OverlaySpec)>,
}

impl State {
    fn served_tags(&self) -> BTreeSet<String> {
        self.served
            .iter()
            .filter_map(|p| self.binds.get(p).cloned())
            .collect()
    }

    /// panel id → the live attachment its comux client is.
    fn bound(&self) -> BTreeMap<String, u64> {
        let Some(clients) = self.model.clients.as_ref() else {
            return BTreeMap::new();
        };
        self.binds
            .iter()
            .filter_map(|(panel, tag)| {
                clients
                    .clients
                    .iter()
                    .filter(|c| &c.tag == tag)
                    .max_by_key(|c| c.id)
                    .map(|c| (panel.clone(), c.id))
            })
            .collect()
    }
}

struct Inner {
    endpoint: Endpoint,
    state: Mutex<State>,
    /// Bumped on every change a GUI could show; GUIs poll it (no callback crosses into a
    /// GUI thread, so tearing a window down can never race a call into freed memory).
    generation: AtomicU64,
    link: Mutex<Option<Arc<Link>>>,
    /// Claim messages produced before the link was installed (`Link::open` starts delivering
    /// the snapshot before it returns); flushed right after installation.
    unsent: Mutex<Vec<ClaimOp>>,
    stop: AtomicBool,
}

/// A running host. Dropping it stops the subscription.
pub struct MuxHost {
    inner: Arc<Inner>,
}

impl MuxHost {
    /// Subscribe to the comux at `endpoint` (reconnecting with backoff for as long as this
    /// lives).
    pub fn start(endpoint: Endpoint) -> Self {
        let inner = Arc::new(Inner {
            endpoint,
            state: Mutex::new(State::default()),
            generation: AtomicU64::new(1),
            link: Mutex::new(None),
            unsent: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
        });
        let i = inner.clone();
        std::thread::Builder::new()
            .name("copad-muxhost".into())
            .spawn(move || run(i))
            .ok();
        MuxHost { inner }
    }

    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::SeqCst)
    }

    /// Register `panel`'s binding tag.
    pub fn bind(&self, panel: &str, tag: &str) {
        self.with(|s| {
            s.binds.insert(panel.into(), tag.into());
        });
    }

    /// Forget a closed panel.
    pub fn unbind(&self, panel: &str) {
        self.with(|s| {
            s.binds.remove(panel);
            s.served.remove(panel);
        });
    }

    /// The panels currently on screen (every split of every window's visible tab).
    pub fn serve(&self, panels: &[String]) {
        self.with(|s| s.served = panels.iter().cloned().collect());
    }

    /// Answer a delegated overlay (`choice` for a picker/confirm, `text` for a prompt; both
    /// `None` = cancelled).
    pub fn answer(&self, overlay: u64, choice: Option<String>, text: Option<String>) {
        if let Ok(mut s) = self.inner.state.lock() {
            s.overlays.retain(|o| o.0 != overlay);
            if let Ok(g) = self.inner.link.lock()
                && let Some(l) = g.as_ref()
            {
                let _ = l.send(&SubMsg::OverlayResult {
                    overlay,
                    choice,
                    text,
                });
            }
        }
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Run a control request (`{"cmd":"select-session",…}`).
    pub fn command(&self, req: Value) {
        if let Some(l) = self.inner.link.lock().ok().and_then(|l| l.clone()) {
            let _ = l.cmd(req);
        }
    }

    /// Everything a GUI draws, as JSON: `connected`, `error`, the topics, `bound`
    /// (panel → attachment) and `claimed` (attachments whose sidebar we took over).
    pub fn state_json(&self) -> Value {
        let Ok(s) = self.inner.state.lock() else {
            return json!({ "connected": false });
        };
        let bound = s.bound();
        let overlays: Vec<Value> = s
            .overlays
            .iter()
            .map(|(id, client, spec)| {
                let panel = bound
                    .iter()
                    .find(|(_, a)| **a == *client)
                    .map(|(p, _)| p.clone());
                json!({ "overlay": id, "client": client, "panel": panel, "spec": spec })
            })
            .collect();
        json!({
            "overlays": overlays,
            "connected": s.connected,
            "error": s.error,
            "tree": s.model.tree,
            "agents": s.model.agents,
            "notifications": s.model.notifications,
            "status": s.model.status,
            "bound": bound,
            "claimed": s.claims.held(),
        })
    }

    /// Apply a GUI-side change, reconcile claims, and bump the generation.
    fn with(&self, f: impl FnOnce(&mut State)) {
        {
            let Ok(mut s) = self.inner.state.lock() else {
                return;
            };
            f(&mut s);
            let tags = s.served_tags();
            let model = s.model.clone();
            let ops = s.claims.reconcile(&model, &tags);
            send_ops_locked(&self.inner, &s, ops);
        }
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
    }
}

impl Drop for MuxHost {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        if let Some(l) = self.inner.link.lock().ok().and_then(|mut l| l.take()) {
            l.close();
        }
    }
}

/// Send claim ops in the order they were reconciled.
///
/// Lock order is ALWAYS state → link: every caller reconciles and sends while holding the state
/// lock, and `run` installs a new link (and flushes what was parked before it existed) holding
/// both. So two reconciliations can never have their messages overtake each other, and an op
/// parked while there was no link is flushed before anything newer is sent (codex P2 r2/r3).
fn send_ops_locked(inner: &Inner, s: &State, ops: Vec<ClaimOp>) {
    if ops.is_empty() {
        return;
    }
    let Ok(guard) = inner.link.lock() else {
        return;
    };
    match guard.as_ref() {
        Some(link) => send_now(link, &s.model.incarnation, ops),
        None => {
            if let Ok(mut u) = inner.unsent.lock() {
                u.extend(ops);
            }
        }
    }
}

fn send_now(link: &Link, incarnation: &str, ops: Vec<ClaimOp>) {
    for op in ops {
        let msg = match op {
            ClaimOp::Ready(attachment) => SubMsg::HostReady {
                attachment,
                incarnation: incarnation.to_string(),
                features: FEATURES.to_vec(),
            },
            ClaimOp::Gone(attachment) => SubMsg::HostGone { attachment },
        };
        let _ = link.send(&msg);
    }
}

fn run(inner: Arc<Inner>) {
    let mut backoff = Duration::from_secs(1);
    while !inner.stop.load(Ordering::SeqCst) {
        let (tx, rx) = std::sync::mpsc::channel();
        let i = inner.clone();
        let opened = Link::open(&inner.endpoint, move |ev| on_event(&i, ev, &tx));
        match opened {
            Ok(link) => {
                backoff = Duration::from_secs(1);
                let link = Arc::new(link);
                // Install and flush holding state → link, the order every sender uses: what
                // was parked goes out before anything reconciled after it.
                if let Ok(s) = inner.state.lock()
                    && let Ok(mut l) = inner.link.lock()
                {
                    *l = Some(link.clone());
                    let early = inner
                        .unsent
                        .lock()
                        .map(|mut u| std::mem::take(&mut *u))
                        .unwrap_or_default();
                    send_now(&link, &s.model.incarnation, early);
                }
                let _ = rx.recv(); // until the stream closes
                if let Ok(mut l) = inner.link.lock() {
                    *l = None;
                }
                if let Ok(mut u) = inner.unsent.lock() {
                    u.clear(); // meant for the connection that just ended
                }
                link.close();
            }
            Err(e) => {
                if let Ok(mut s) = inner.state.lock() {
                    s.connected = false;
                    s.error = Some(e.to_string());
                }
                inner.generation.fetch_add(1, Ordering::SeqCst);
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
        std::thread::sleep(backoff);
    }
}

fn on_event(inner: &Arc<Inner>, ev: LinkEvent, closed: &std::sync::mpsc::Sender<()>) {
    {
        let Ok(mut s) = inner.state.lock() else {
            return;
        };
        match &ev {
            LinkEvent::Msg(PubMsg::Error { .. }) => s.claims.refused(),
            LinkEvent::Msg(PubMsg::Overlay {
                overlay,
                client,
                spec,
            }) => {
                if s.claims.covers(*client) {
                    s.overlays.push((*overlay, *client, spec.clone()));
                }
            }
            LinkEvent::Msg(PubMsg::OverlayClosed { overlay }) => {
                s.overlays.retain(|o| o.0 != *overlay);
            }
            LinkEvent::Msg(m) => {
                if s.model.apply(m) == Applied::Welcome {
                    // A new server: every attachment id and claim we held is meaningless.
                    s.claims = Claims::default();
                    s.overlays.clear();
                    s.connected = true;
                    s.error = None;
                }
                let tags = s.served_tags();
                let model = s.model.clone();
                let ops = s.claims.reconcile(&model, &tags);
                send_ops_locked(inner, &s, ops);
            }
            LinkEvent::Closed(why) => {
                s.connected = false;
                s.error = Some(why.clone());
                s.model = Model::default();
                s.claims = Claims::default();
                s.overlays.clear();
            }
        }
    }
    inner.generation.fetch_add(1, Ordering::SeqCst);
    if matches!(ev, LinkEvent::Closed(_)) {
        let _ = closed.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copad_mux_proto::{Agents, ClientInfo, Clients, Tree};

    fn model(clients: Vec<ClientInfo>, with_lists: bool) -> Model {
        let mut m = Model::default();
        let mut topics = BTreeMap::new();
        let mut put = |name: &str, v: Value| {
            topics.insert(
                name.to_string(),
                copad_mux_proto::TopicValue { rev: 1, data: v },
            );
        };
        put(
            "clients",
            serde_json::to_value(Clients {
                clients,
                hidden: vec![],
            })
            .unwrap(),
        );
        if with_lists {
            put("tree", serde_json::to_value(Tree::default()).unwrap());
            put("agents", serde_json::to_value(Agents::default()).unwrap());
            put(
                "status",
                serde_json::to_value(copad_mux_proto::Status::default()).unwrap(),
            );
        }
        m.apply(&PubMsg::Snapshot { rev: 1, topics });
        m
    }

    fn client(id: u64, tag: &str, claimed: &[Feature]) -> ClientInfo {
        ClientInfo {
            id,
            tag: tag.into(),
            cols: 80,
            rows: 24,
            claimed: claimed.to_vec(),
        }
    }

    fn tags(t: &[&str]) -> BTreeSet<String> {
        t.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_served_tab_is_claimed_once_and_held_when_confirmed() {
        let mut c = Claims::default();
        let m = model(vec![client(3, "copad:p1@s", &[]), client(4, "", &[])], true);
        assert_eq!(c.reconcile(&m, &tags(&["copad:p1@s"])), [ClaimOp::Ready(3)]);
        assert_eq!(
            c.reconcile(&m, &tags(&["copad:p1@s"])),
            [],
            "one claim in flight"
        );
        let m = model(vec![client(3, "copad:p1@s", &FEATURES)], true);
        assert_eq!(c.reconcile(&m, &tags(&["copad:p1@s"])), []);
        assert!(c.held().contains(&3));
    }

    #[test]
    fn an_unserved_tab_or_missing_data_releases_the_claim() {
        let mut c = Claims::default();
        let m = model(vec![client(3, "t", &[])], true);
        c.reconcile(&m, &tags(&["t"]));
        let held = model(vec![client(3, "t", &FEATURES)], true);
        c.reconcile(&held, &tags(&["t"]));
        // The tab went to the background.
        assert_eq!(c.reconcile(&held, &tags(&[])), [ClaimOp::Gone(3)]);
        // Served again, but the lists are gone (unavailable): do not claim.
        let no_lists = model(vec![client(3, "t", &[])], false);
        assert_eq!(c.reconcile(&no_lists, &tags(&["t"])), []);
        // Data back: claim again.
        assert_eq!(c.reconcile(&m, &tags(&["t"])), [ClaimOp::Ready(3)]);
    }

    #[test]
    fn a_refused_claim_waits_until_the_other_owner_is_gone() {
        let mut c = Claims::default();
        let owned = model(vec![client(3, "t", &[])], true);
        assert_eq!(c.reconcile(&owned, &tags(&["t"])), [ClaimOp::Ready(3)]);
        c.refused();
        let other = model(vec![client(3, "t", &FEATURES)], true);
        assert_eq!(
            c.reconcile(&other, &tags(&["t"])),
            [],
            "someone else holds it"
        );
        // The other host disconnected: the claim list is empty again.
        assert_eq!(c.reconcile(&owned, &tags(&["t"])), [ClaimOp::Ready(3)]);
    }

    #[test]
    fn a_refusal_that_is_not_about_ownership_does_not_loop() {
        let mut c = Claims::default();
        let m = model(vec![client(3, "t", &[])], true);
        let mut sent = 0;
        for _ in 0..10 {
            sent += c.reconcile(&m, &tags(&["t"])).len();
            c.refused();
        }
        assert_eq!(sent, MAX_ATTEMPTS as usize);
    }

    #[test]
    fn repeated_successful_claims_never_exhaust_the_budget() {
        let mut c = Claims::default();
        let free = model(vec![client(3, "t", &[])], true);
        let ours = model(vec![client(3, "t", &FEATURES)], true);
        for _ in 0..5 {
            assert_eq!(c.reconcile(&free, &tags(&["t"])), [ClaimOp::Ready(3)]);
            c.reconcile(&ours, &tags(&["t"]));
            assert_eq!(c.reconcile(&ours, &tags(&[])), [ClaimOp::Gone(3)]);
        }
    }

    #[test]
    fn losing_the_clients_topic_releases_instead_of_forgetting() {
        let mut c = Claims::default();
        c.reconcile(&model(vec![client(3, "t", &[])], true), &tags(&["t"]));
        c.reconcile(&model(vec![client(3, "t", &FEATURES)], true), &tags(&["t"]));
        let mut gone = Model::default();
        gone.apply(&PubMsg::Unavailable {
            topic: "clients".into(),
            rev: 9,
            reason: "big".into(),
        });
        assert_eq!(c.reconcile(&gone, &tags(&["t"])), [ClaimOp::Gone(3)]);
        assert!(c.held().is_empty());
    }

    #[test]
    fn a_detached_attachment_is_forgotten_without_a_release() {
        let mut c = Claims::default();
        c.reconcile(&model(vec![client(3, "t", &[])], true), &tags(&["t"]));
        c.reconcile(&model(vec![client(3, "t", &FEATURES)], true), &tags(&["t"]));
        assert_eq!(c.reconcile(&model(vec![], true), &tags(&["t"])), []);
        assert!(c.held().is_empty());
    }
}

// ---------------------------------------------------------------------------------------------
// Remote comux (decisions #171): copad opens the remote session itself, so it knows which tab
// is which host — no guessing from what a hand-typed `ssh` printed.
// ---------------------------------------------------------------------------------------------

/// An ssh destination copad will put on a command line: a `~/.ssh/config` alias or
/// `[user@]host[:port]`-shaped token. Anything that could be read as an option or break out of
/// the quoting is refused rather than escaped.
pub fn valid_dest(dest: &str) -> bool {
    !dest.is_empty()
        && !dest.starts_with('-')
        && dest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@._:-[]%".contains(c))
}

/// The remote comux command (default `comux`): may carry arguments and an `env` prefix, but no
/// quotes, backslashes, `$`, backticks, `;`, `&`, `|` or newlines — it is spliced into quoted
/// remote shell text.
pub fn valid_comux(cmd: &str) -> bool {
    !cmd.trim().is_empty() && !cmd.chars().any(|c| "'\"\\$`;&|<>\n\r".contains(c))
}

/// The ssh program copad runs for remote comux: `COPAD_SSH` (a wrapper, `tsh`, an ssh outside
/// PATH) or `ssh`. An absolute path also survives a tab shell that rebuilds `PATH` in its rc
/// files. A value that could break the command line is ignored.
pub fn ssh_program() -> String {
    std::env::var("COPAD_SSH")
        .ok()
        .filter(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "/._-+~".contains(c))
        })
        .unwrap_or_else(|| "ssh".into())
}

/// A fresh binding tag for a remote tab.
pub fn new_remote_tag() -> String {
    let mut b = [0u8; 12];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut b);
    }
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("copad-remote:{hex}")
}

/// What the copad tab runs to attach the remote comux client with `tag`: through a login
/// shell (so a comux in `~/.local/bin` is found) with the tag in its environment — the remote
/// client reports it, which is how the L0 connection finds the attachment. `None` when the
/// destination or command is not acceptable.
pub fn remote_attach_command(dest: &str, comux: &str, tag: &str) -> Option<String> {
    if !valid_dest(dest) || !valid_comux(comux) || !valid_comux(tag) {
        return None;
    }
    Some(format!(
        "{} -t {dest} 'exec env COPAD_MUX_HOST_TAG={tag} \"${{SHELL:-/bin/sh}}\" -lc \"exec {comux}\"'",
        ssh_program()
    ))
}

/// The L0 endpoint for the same remote comux.
pub fn remote_endpoint(dest: &str, comux: &str) -> Option<Endpoint> {
    if !valid_dest(dest) || !valid_comux(comux) {
        return None;
    }
    Some(match Endpoint::ssh_with(dest, comux) {
        Endpoint::Command { args, .. } => Endpoint::Command {
            program: ssh_program(),
            args,
        },
        other => other,
    })
}

#[cfg(test)]
mod remote_tests {
    use super::*;

    #[test]
    fn destinations_that_could_be_options_or_escape_quoting_are_refused() {
        for ok in ["box", "me@box.tail.ts.net", "box:2222", "a_b-c.d", "[::1]"] {
            assert!(valid_dest(ok), "{ok}");
        }
        for bad in ["", "-oProxyCommand=x", "a b", "a;b", "a'b", "$(x)", "a\nb"] {
            assert!(!valid_dest(bad), "{bad:?}");
        }
        assert!(valid_comux("env COPAD_MUX_SOCK=/tmp/x/s /home/u/comux"));
        for bad in ["comux'", "comux; rm -rf ~", "$(x)", "`x`", "a\\b", "a | b"] {
            assert!(!valid_comux(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_attach_command_carries_the_tag_through_a_login_shell() {
        let c = remote_attach_command("box", "comux", "copad-remote:ab12").unwrap();
        assert_eq!(
            c,
            r#"ssh -t box 'exec env COPAD_MUX_HOST_TAG=copad-remote:ab12 "${SHELL:-/bin/sh}" -lc "exec comux"'"#
        );
        assert!(remote_attach_command("-oX", "comux", "t").is_none());
        let t = new_remote_tag();
        assert!(t.starts_with("copad-remote:") && t.len() == "copad-remote:".len() + 24);
        assert_ne!(t, new_remote_tag());
    }

    #[test]
    fn the_proxy_endpoint_uses_the_same_remote_command() {
        let Some(Endpoint::Command { program, args }) = remote_endpoint("box", "env A=1 /x/comux")
        else {
            panic!()
        };
        assert_eq!(program, "ssh");
        assert_eq!(
            args.last().unwrap(),
            r#"exec "${SHELL:-/bin/sh}" -lc 'exec env A=1 /x/comux proxy --stdio'"#
        );
    }
}
