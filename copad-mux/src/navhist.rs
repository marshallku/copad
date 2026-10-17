//! Back / forward / last navigation history across sessions and tabs (decisions #140).
//!
//! A browser-style pair of stacks around the current location, fed by OBSERVATION rather than
//! by every focus path calling in: the view can move from ~25 places (keys, clicks, the switcher,
//! toast jumps, control verbs, reaping), and a hook per path is exactly how one gets forgotten.
//! `App` observes the landed location once per input it handles instead.
//!
//! Only a change of `(session, tab)` is a JUMP. Moving between panes of one tab just updates the
//! current entry's pane, so `back` lands on the pane you last used there without `h/j/k/l`
//! flooding the list.
//!
//! State-free on purpose: validity is the caller's question (`valid`), so this module never sees
//! `State` and every rule is unit-testable on plain ids.

use crate::model::{TabId, TerminalId, WorkspaceId};

/// The most entries either stack keeps.
pub const CAP: usize = 50;

/// One place the user was: a tab of a session, and the pane focused there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loc {
    pub ws: WorkspaceId,
    pub tab: TabId,
    pub term: Option<TerminalId>,
}

impl Loc {
    /// Whether `other` is the same jump target (pane aside).
    fn same_place(&self, other: &Loc) -> bool {
        self.ws == other.ws && self.tab == other.tab
    }
}

#[derive(Debug, Default)]
pub struct NavHistory {
    back: Vec<Loc>,
    forward: Vec<Loc>,
    cur: Option<Loc>,
}

impl NavHistory {
    pub fn current(&self) -> Option<&Loc> {
        self.cur.as_ref()
    }

    pub fn back_len(&self) -> usize {
        self.back.len()
    }

    pub fn forward_len(&self) -> usize {
        self.forward.len()
    }

    /// Record that the view is now at `now`. Returns whether that was a jump (a new entry).
    pub fn observe(&mut self, now: Loc) -> bool {
        match self.cur.take() {
            None => {
                self.cur = Some(now);
                false
            }
            Some(cur) if cur.same_place(&now) => {
                self.cur = Some(now);
                false
            }
            Some(cur) => {
                push_dedup(&mut self.back, cur);
                self.forward.clear();
                self.cur = Some(now);
                true
            }
        }
    }

    /// Step back to the newest still-valid entry, moving the current one onto `forward`.
    pub fn back(&mut self, valid: impl Fn(&Loc) -> bool) -> Option<Loc> {
        let target = pop_valid(&mut self.back, self.cur.as_ref(), &valid)?;
        if let Some(cur) = self.cur.take() {
            push_dedup(&mut self.forward, cur);
        }
        self.cur = Some(target.clone());
        Some(target)
    }

    /// Step forward again — the mirror of [`Self::back`].
    pub fn forward(&mut self, valid: impl Fn(&Loc) -> bool) -> Option<Loc> {
        let target = pop_valid(&mut self.forward, self.cur.as_ref(), &valid)?;
        if let Some(cur) = self.cur.take() {
            push_dedup(&mut self.back, cur);
        }
        self.cur = Some(target.clone());
        Some(target)
    }

    /// Alt-Tab: go to the previous location as a NEW jump, so pressing it again comes back.
    pub fn last(&mut self, valid: impl Fn(&Loc) -> bool) -> Option<Loc> {
        let target = pop_valid(&mut self.back, self.cur.as_ref(), &valid)?;
        self.observe(target.clone());
        Some(target)
    }

    /// Drop every entry that no longer exists, and normalize the current one.
    pub fn prune(&mut self, valid: impl Fn(&Loc) -> bool) {
        self.back.retain(|l| valid(l));
        self.forward.retain(|l| valid(l));
        if self.cur.as_ref().is_some_and(|l| !valid(l)) {
            self.cur = None;
        }
    }

    /// Put `now` in place as the current location WITHOUT recording a jump — after a
    /// back/forward landed somewhere slightly different from the entry (its pane died).
    pub fn settle(&mut self, now: Loc) {
        self.cur = Some(now);
    }
}

/// Push `loc`, removing any older entry for the same place and keeping at most [`CAP`].
fn push_dedup(stack: &mut Vec<Loc>, loc: Loc) {
    stack.retain(|l| !l.same_place(&loc));
    stack.push(loc);
    if stack.len() > CAP {
        stack.remove(0);
    }
}

/// Pop the newest entry that is alive AND somewhere else: an entry for where the view
/// already is would "succeed" without moving (the fallback after killing the attached session
/// is typically the top of `back`), so it is dropped like a dead one.
fn pop_valid(
    stack: &mut Vec<Loc>,
    cur: Option<&Loc>,
    valid: &impl Fn(&Loc) -> bool,
) -> Option<Loc> {
    while let Some(l) = stack.pop() {
        if valid(&l) && !cur.is_some_and(|c| c.same_place(&l)) {
            return Some(l);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(ws: &str, tab: &str) -> Loc {
        Loc {
            ws: WorkspaceId::new(ws),
            tab: TabId::new(format!("{ws}:{tab}")),
            term: None,
        }
    }

    fn with_term(mut l: Loc, t: &str) -> Loc {
        l.term = Some(TerminalId::new(t));
        l
    }

    fn walk(h: &mut NavHistory, places: &[(&str, &str)]) {
        for (w, t) in places {
            h.observe(loc(w, t));
        }
    }

    fn ws_of(l: Option<Loc>) -> Option<String> {
        l.map(|l| l.ws.to_string())
    }

    #[test]
    fn the_first_observation_only_seeds() {
        let mut h = NavHistory::default();
        assert!(!h.observe(loc("a", "t0")));
        assert_eq!(h.back_len(), 0);
    }

    #[test]
    fn a_pane_change_in_the_same_tab_is_not_a_jump_but_is_remembered() {
        let mut h = NavHistory::default();
        h.observe(with_term(loc("a", "t0"), "term0"));
        assert!(!h.observe(with_term(loc("a", "t0"), "term1")));
        h.observe(loc("b", "t0"));
        let back = h.back(|_| true).unwrap();
        assert_eq!(
            back.term,
            Some(TerminalId::new("term1")),
            "lands on the pane last used"
        );
    }

    #[test]
    fn back_and_forward_walk_the_trail() {
        let mut h = NavHistory::default();
        walk(&mut h, &[("a", "t0"), ("b", "t0"), ("c", "t0")]);
        assert_eq!(ws_of(h.back(|_| true)).as_deref(), Some("b"));
        assert_eq!(ws_of(h.back(|_| true)).as_deref(), Some("a"));
        assert!(h.back(|_| true).is_none(), "the start of the trail");
        assert_eq!(ws_of(h.forward(|_| true)).as_deref(), Some("b"));
        assert_eq!(ws_of(h.forward(|_| true)).as_deref(), Some("c"));
        assert!(h.forward(|_| true).is_none());
    }

    #[test]
    fn a_new_jump_after_going_back_drops_forward() {
        let mut h = NavHistory::default();
        walk(&mut h, &[("a", "t0"), ("b", "t0"), ("c", "t0")]);
        h.back(|_| true);
        h.observe(loc("d", "t0"));
        assert_eq!(h.forward_len(), 0);
    }

    #[test]
    fn last_toggles_between_two_places() {
        let mut h = NavHistory::default();
        walk(&mut h, &[("a", "t0"), ("b", "t0"), ("c", "t0")]);
        assert_eq!(ws_of(h.last(|_| true)).as_deref(), Some("b"));
        assert_eq!(ws_of(h.last(|_| true)).as_deref(), Some("c"));
        assert_eq!(ws_of(h.last(|_| true)).as_deref(), Some("b"));
        // `a` is still reachable behind the toggle pair.
        assert_eq!(ws_of(h.back(|_| true)).as_deref(), Some("c"));
        assert_eq!(ws_of(h.back(|_| true)).as_deref(), Some("a"));
    }

    #[test]
    fn revisiting_a_place_moves_it_instead_of_duplicating_it() {
        let mut h = NavHistory::default();
        walk(
            &mut h,
            &[("a", "t0"), ("b", "t0"), ("a", "t0"), ("c", "t0")],
        );
        assert_eq!(h.back_len(), 2, "a and b once each");
        assert_eq!(ws_of(h.back(|_| true)).as_deref(), Some("a"));
        assert_eq!(ws_of(h.back(|_| true)).as_deref(), Some("b"));
    }

    #[test]
    fn dead_entries_are_skipped_and_pruned() {
        let mut h = NavHistory::default();
        walk(&mut h, &[("a", "t0"), ("b", "t0"), ("c", "t0")]);
        let alive = |l: &Loc| l.ws.to_string() != "b";
        assert_eq!(ws_of(h.back(alive)).as_deref(), Some("a"), "b was skipped");
        let mut h = NavHistory::default();
        walk(&mut h, &[("a", "t0"), ("b", "t0"), ("c", "t0")]);
        h.prune(alive);
        assert_eq!(h.back_len(), 1);
        h.prune(|l| l.ws.to_string() == "a");
        assert!(h.current().is_none(), "a dead current is dropped");
    }

    #[test]
    fn an_entry_for_where_the_view_already_is_is_skipped() {
        // a → b → c, then c dies and the view falls back to b, which is still atop `back`.
        let mut h = NavHistory::default();
        walk(&mut h, &[("a", "t0"), ("b", "t0"), ("c", "t0")]);
        let alive = |l: &Loc| l.ws.to_string() != "c";
        h.prune(alive);
        h.observe(loc("b", "t0"));
        assert_eq!(ws_of(h.back(alive)).as_deref(), Some("a"));
        let mut h = NavHistory::default();
        walk(&mut h, &[("a", "t0"), ("b", "t0"), ("c", "t0")]);
        h.prune(alive);
        h.observe(loc("b", "t0"));
        assert_eq!(ws_of(h.last(alive)).as_deref(), Some("a"));
    }

    #[test]
    fn the_stacks_are_capped() {
        let mut h = NavHistory::default();
        for i in 0..(CAP + 20) {
            h.observe(loc("a", &format!("t{i}")));
        }
        assert_eq!(h.back_len(), CAP);
        // The oldest fell off; the newest are kept.
        let mut seen = 0;
        while h.back(|_| true).is_some() {
            seen += 1;
        }
        assert_eq!(seen, CAP);
        // 70 places, the newest is current: the oldest 50-back is t19.
        assert_eq!(h.current().unwrap().tab.to_string(), "a:t19");
    }
}
