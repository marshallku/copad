//! Authoritative-state data model (agent-mux-spec.md §1).
//!
//! Pure data + tree helpers, no PTYs and no I/O. Every mutable entity carries a
//! monotonic [`Rev`] bumped on each change; the single authoritative actor
//! ([`crate::state::State`]) owns all instances and is the only writer.

use serde::{Deserialize, Serialize};

/// Monotonic revision. Bumped on every change to its entity so optimistic
/// concurrency (`if_rev`) and event ordering are well-defined (spec §1, §4a).
pub type Rev = u64;

macro_rules! string_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub String);
        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_id!(WorkspaceId, "A workspace (project/task container).");
string_id!(TabId, "A tab (one BSP layout inside a workspace).");
string_id!(
    PaneId,
    "A layout slot in a tab's split tree. Distinct from `TerminalId`."
);
string_id!(
    TerminalId,
    "A live PTY runtime. A pane can move while its terminal stays alive."
);

/// An attached surface connection. `u64` because clients are ephemeral and
/// server-minted (spec §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientId(pub u64);

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "c{}", self.0)
    }
}

/// A client's role on a workspace. At most one `Controller` per workspace
/// (invariant G1); `Controller` owns geometry + input (spec §2, §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Controller,
    Observer,
}

/// Split direction (BSP). `Right` = a vertical divider (side by side), `Down` =
/// a horizontal divider (stacked). Matches the herdr/session-v3 vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    Right,
    Down,
}

/// Rolled-up agent state (spec §8; classification authority is `tmx`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

/// A live terminal (PTY runtime placeholder in the pure model). `cols`/`rows`
/// are DERIVED from the controller viewport + layout (invariant G2); nothing but
/// a controller resize changes them (G3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Terminal {
    pub id: TerminalId,
    pub cols: u16,
    pub rows: u16,
    pub agent: AgentState,
    pub rev: Rev,
}

/// A BSP split tree: a typed leaf (pane ↔ terminal) or a branch of two subtrees.
/// `ratio` is a normalized 0..1 divider position (never pixels). Reuses the
/// session-v3 shape (#64).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "node", rename_all = "snake_case")]
pub enum SplitTree {
    Leaf {
        pane: PaneId,
        terminal: TerminalId,
    },
    Branch {
        dir: Dir,
        ratio: f32,
        first: Box<SplitTree>,
        second: Box<SplitTree>,
    },
}

/// A layout description used to REBUILD a [`SplitTree`] at restore time. It carries the
/// tree SHAPE (branch dirs + ratios) but no ids — the builder mints fresh pane/terminal
/// ids per leaf and reports them back in DFS pre-order. (Persistence keeps the runtime
/// ids out of the on-disk snapshot; they're regenerated on restore.)
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutSpec {
    Leaf,
    Branch {
        dir: Dir,
        ratio: f32,
        first: Box<LayoutSpec>,
        second: Box<LayoutSpec>,
    },
}

/// A rectangle in terminal cells, used to derive per-pane geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub cols: u16,
    pub rows: u16,
}

impl SplitTree {
    /// DFS pre-order pane ids.
    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect_panes(&mut out);
        out
    }

    fn collect_panes(&self, out: &mut Vec<PaneId>) {
        match self {
            SplitTree::Leaf { pane, .. } => out.push(pane.clone()),
            SplitTree::Branch { first, second, .. } => {
                first.collect_panes(out);
                second.collect_panes(out);
            }
        }
    }

    /// The leftmost (DFS pre-order) leaf's pane id.
    pub fn leftmost_pane(&self) -> PaneId {
        match self {
            SplitTree::Leaf { pane, .. } => pane.clone(),
            SplitTree::Branch { first, .. } => first.leftmost_pane(),
        }
    }

    /// Find the terminal id for a pane, if present.
    pub fn terminal_of(&self, target: &PaneId) -> Option<&TerminalId> {
        match self {
            SplitTree::Leaf { pane, terminal } => (pane == target).then_some(terminal),
            SplitTree::Branch { first, second, .. } => first
                .terminal_of(target)
                .or_else(|| second.terminal_of(target)),
        }
    }

    pub fn contains(&self, target: &PaneId) -> bool {
        self.terminal_of(target).is_some()
    }

    /// Replace the leaf holding `target` with a branch splitting `dir`, keeping
    /// the original leaf as `first` and inserting a new leaf as `second`.
    /// Returns false if `target` was not found.
    pub fn split_leaf(
        &mut self,
        target: &PaneId,
        dir: Dir,
        new_pane: PaneId,
        new_term: TerminalId,
    ) -> bool {
        match self {
            SplitTree::Leaf { pane, terminal } if pane == target => {
                let first = SplitTree::Leaf {
                    pane: pane.clone(),
                    terminal: terminal.clone(),
                };
                let second = SplitTree::Leaf {
                    pane: new_pane,
                    terminal: new_term,
                };
                *self = SplitTree::Branch {
                    dir,
                    ratio: 0.5,
                    first: Box::new(first),
                    second: Box::new(second),
                };
                true
            }
            SplitTree::Leaf { .. } => false,
            SplitTree::Branch { first, second, .. } => {
                first.split_leaf(target, dir, new_pane.clone(), new_term.clone())
                    || second.split_leaf(target, dir, new_pane, new_term)
            }
        }
    }

    /// Remove the leaf holding `target`, collapsing its parent branch into the
    /// sibling. Returns the removed terminal id, or `None` if not found or if
    /// `target` is the whole tree (the caller must reject closing the last pane).
    pub fn remove_leaf(&mut self, target: &PaneId) -> Option<TerminalId> {
        // The root itself being the target cannot be handled here (no parent to
        // collapse into); the caller checks `is_last`.
        if let SplitTree::Leaf { .. } = self {
            return None;
        }
        self.remove_leaf_inner(target)
    }

    fn remove_leaf_inner(&mut self, target: &PaneId) -> Option<TerminalId> {
        let SplitTree::Branch { first, second, .. } = self else {
            return None;
        };
        // Is a direct child the target leaf?
        if let SplitTree::Leaf { pane, terminal } = first.as_ref()
            && pane == target
        {
            let term = terminal.clone();
            *self = (**second).clone();
            return Some(term);
        }
        if let SplitTree::Leaf { pane, terminal } = second.as_ref()
            && pane == target
        {
            let term = terminal.clone();
            *self = (**first).clone();
            return Some(term);
        }
        // Recurse.
        first
            .remove_leaf_inner(target)
            .or_else(|| second.remove_leaf_inner(target))
    }

    pub fn is_single_leaf(&self) -> bool {
        matches!(self, SplitTree::Leaf { .. })
    }

    /// Nudge the divider of the DEEPEST ancestor branch on `axis` that contains
    /// `pane`, growing (or shrinking) `pane`'s share by `step` (a 0..1 fraction of
    /// that branch's extent). Returns false if no such branch exists (e.g. a single
    /// pane, or no split on that axis). The ratio is clamped to `[0.05, 0.95]` so
    /// geometry still conserves (G2).
    pub fn resize(&mut self, pane: &PaneId, axis: Dir, grow: bool, step: f32) -> bool {
        let SplitTree::Branch {
            dir,
            ratio,
            first,
            second,
        } = self
        else {
            return false;
        };
        // Deepest-first: let a nested branch on this axis handle it before we do.
        if first.resize(pane, axis, grow, step) || second.resize(pane, axis, grow, step) {
            return true;
        }
        if *dir == axis {
            let in_first = first.contains(pane);
            let in_second = second.contains(pane);
            if in_first || in_second {
                // `ratio` is first's share. Growing the side holding `pane` means
                // +step when it's `first`, -step when it's `second`.
                let delta = if in_first == grow { step } else { -step };
                let new = (*ratio + delta).clamp(0.05, 0.95);
                // Already at the clamp boundary → no real change (don't churn revs).
                if (new - *ratio).abs() <= f32::EPSILON {
                    return false;
                }
                *ratio = new;
                return true;
            }
        }
        false
    }

    /// Split an extent (cols for `Right`, rows for `Down`) into
    /// `(first, second, divider)` such that `first + second + divider == extent`
    /// EXACTLY — cells are conserved with no over-allocation. The divider costs 1
    /// cell only when there is room for it (`extent >= 1`); a 0-extent branch
    /// cannot show a divider, so it costs 0. A pane can reach 0 cells (a rendering
    /// concern — PTY spawn clamps to a minimum — not a state-model concern).
    fn split3(extent: u16, ratio: f32) -> (u16, u16, u16) {
        let divider = if extent >= 1 { 1 } else { 0 };
        let usable = extent - divider;
        let r = ratio.clamp(0.05, 0.95);
        let a = (((usable as f32) * r).round() as u16).min(usable);
        (a, usable - a, divider)
    }

    /// Assign a derived cell size to each terminal, tiling `area` by ratios with a
    /// 1-cell divider per (non-degenerate) branch. Cells are **conserved** — the
    /// tree's footprint equals `area` exactly (G2), so no leaf and no sum can
    /// exceed the viewport.
    pub fn derive_sizes(&self, area: Rect, out: &mut Vec<(TerminalId, u16, u16)>) {
        match self {
            SplitTree::Leaf { terminal, .. } => {
                out.push((terminal.clone(), area.cols, area.rows));
            }
            SplitTree::Branch {
                dir,
                ratio,
                first,
                second,
                ..
            } => match dir {
                Dir::Right => {
                    let (a, b, _div) = Self::split3(area.cols, *ratio);
                    first.derive_sizes(
                        Rect {
                            cols: a,
                            rows: area.rows,
                        },
                        out,
                    );
                    second.derive_sizes(
                        Rect {
                            cols: b,
                            rows: area.rows,
                        },
                        out,
                    );
                }
                Dir::Down => {
                    let (a, b, _div) = Self::split3(area.rows, *ratio);
                    first.derive_sizes(
                        Rect {
                            cols: area.cols,
                            rows: a,
                        },
                        out,
                    );
                    second.derive_sizes(
                        Rect {
                            cols: area.cols,
                            rows: b,
                        },
                        out,
                    );
                }
            },
        }
    }

    /// The bounding box the tree occupies when tiled into `area`. With the
    /// conserving `derive_sizes`, this equals `area` exactly — the invariant a
    /// geometry test asserts (G2: nothing derived outside the viewport).
    pub fn footprint(&self, area: Rect) -> Rect {
        match self {
            SplitTree::Leaf { .. } => area,
            SplitTree::Branch {
                dir,
                ratio,
                first,
                second,
            } => match dir {
                Dir::Right => {
                    let (a, b, div) = Self::split3(area.cols, *ratio);
                    let f = first.footprint(Rect {
                        cols: a,
                        rows: area.rows,
                    });
                    let s = second.footprint(Rect {
                        cols: b,
                        rows: area.rows,
                    });
                    Rect {
                        cols: f.cols + div + s.cols,
                        rows: f.rows.max(s.rows),
                    }
                }
                Dir::Down => {
                    let (a, b, div) = Self::split3(area.rows, *ratio);
                    let f = first.footprint(Rect {
                        cols: area.cols,
                        rows: a,
                    });
                    let s = second.footprint(Rect {
                        cols: area.cols,
                        rows: b,
                    });
                    Rect {
                        cols: f.cols.max(s.cols),
                        rows: f.rows + div + s.rows,
                    }
                }
            },
        }
    }
}

/// A pane's placed rectangle (position + size in cells) for rendering. Derived
/// from a split tree tiled into an area; dividers occupy the 1-cell gaps between
/// adjacent rects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRect {
    pub terminal: TerminalId,
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
}

impl SplitTree {
    /// Place every leaf's terminal at an absolute `(x, y)` + size, tiling the
    /// `(x, y, cols, rows)` area. Uses the same conserving `split3` as
    /// `derive_sizes`, so the rects + 1-cell dividers exactly fill the area with
    /// no overlap (the divider sits in the gap between the two children).
    pub fn derive_layout(&self, x: u16, y: u16, cols: u16, rows: u16, out: &mut Vec<PaneRect>) {
        match self {
            SplitTree::Leaf { terminal, .. } => out.push(PaneRect {
                terminal: terminal.clone(),
                x,
                y,
                cols,
                rows,
            }),
            SplitTree::Branch {
                dir,
                ratio,
                first,
                second,
            } => match dir {
                Dir::Right => {
                    let (a, b, div) = Self::split3(cols, *ratio);
                    first.derive_layout(x, y, a, rows, out);
                    second.derive_layout(x + a + div, y, b, rows, out);
                }
                Dir::Down => {
                    let (a, b, div) = Self::split3(rows, *ratio);
                    first.derive_layout(x, y, cols, a, out);
                    second.derive_layout(x, y + a + div, cols, b, out);
                }
            },
        }
    }
}

/// A tab: one BSP layout + a focused pane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub id: TabId,
    pub name: Option<String>,
    pub layout: SplitTree,
    pub focused: PaneId,
    pub rev: Rev,
}

/// A workspace: ordered tabs + the control lease (≤1 controller, invariant G1) +
/// the controller's viewport (drives geometry, G2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: Option<String>,
    pub tabs: Vec<Tab>,
    pub active_tab: TabId,
    pub controller: Option<ClientId>,
    pub viewport: Rect,
    pub rev: Rev,
}

impl Workspace {
    pub fn tab(&self, id: &TabId) -> Option<&Tab> {
        self.tabs.iter().find(|t| &t.id == id)
    }
    pub fn tab_mut(&mut self, id: &TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| &t.id == id)
    }
    /// The tab whose layout contains `pane`.
    pub fn tab_of_pane(&self, pane: &PaneId) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.layout.contains(pane))
    }
    pub fn tab_of_pane_mut(&mut self, pane: &PaneId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.layout.contains(pane))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(p: &str, t: &str) -> SplitTree {
        SplitTree::Leaf {
            pane: PaneId::new(p),
            terminal: TerminalId::new(t),
        }
    }

    #[test]
    fn derive_layout_tiles_a_right_split_with_a_divider_gap() {
        let tree = SplitTree::Branch {
            dir: Dir::Right,
            ratio: 0.5,
            first: Box::new(leaf("p0", "t0")),
            second: Box::new(leaf("p1", "t1")),
        };
        let mut out = Vec::new();
        tree.derive_layout(0, 0, 80, 24, &mut out);
        assert_eq!(out.len(), 2);
        let (a, b) = (&out[0], &out[1]);
        assert_eq!((a.x, a.y, a.rows), (0, 0, 24));
        assert_eq!((b.y, b.rows), (0, 24));
        assert_eq!(b.x, a.cols + 1, "second pane sits after the 1-cell divider");
        assert_eq!(a.cols + 1 + b.cols, 80, "widths + divider tile the area");
    }

    #[test]
    fn resize_nudges_the_nearest_axis_branch_and_conserves() {
        // p0 | p1  (a horizontal / Right split)
        let mut tree = SplitTree::Branch {
            dir: Dir::Right,
            ratio: 0.5,
            first: Box::new(leaf("p0", "t0")),
            second: Box::new(leaf("p1", "t1")),
        };
        // grow p0 (left pane) to the right → ratio increases
        assert!(tree.resize(&PaneId::new("p0"), Dir::Right, true, 0.1));
        let SplitTree::Branch { ratio, .. } = &tree else {
            panic!()
        };
        assert!((*ratio - 0.6).abs() < 1e-4, "p0 grew: ratio 0.5→0.6");
        // a vertical resize has no Down branch here → no-op
        assert!(!tree.resize(&PaneId::new("p0"), Dir::Down, true, 0.1));
        // geometry still conserves at the new ratio
        let area = Rect {
            cols: 100,
            rows: 40,
        };
        assert_eq!(tree.footprint(area), area);
    }

    #[test]
    fn derive_layout_agrees_with_sizes_and_stays_in_bounds() {
        let tree = SplitTree::Branch {
            dir: Dir::Down,
            ratio: 0.5,
            first: Box::new(SplitTree::Branch {
                dir: Dir::Right,
                ratio: 0.5,
                first: Box::new(leaf("p0", "t0")),
                second: Box::new(leaf("p1", "t1")),
            }),
            second: Box::new(leaf("p2", "t2")),
        };
        let area = Rect {
            cols: 100,
            rows: 40,
        };
        assert_eq!(tree.footprint(area), area);
        let mut rects = Vec::new();
        tree.derive_layout(0, 0, area.cols, area.rows, &mut rects);
        assert_eq!(rects.len(), 3);
        for r in &rects {
            assert!(r.x + r.cols <= area.cols, "rect within viewport width");
            assert!(r.y + r.rows <= area.rows, "rect within viewport height");
        }
        // derive_layout and derive_sizes agree on each terminal's size.
        let mut sizes = Vec::new();
        tree.derive_sizes(area, &mut sizes);
        for (tid, c, rr) in &sizes {
            let rect = rects.iter().find(|p| &p.terminal == tid).unwrap();
            assert_eq!(
                (rect.cols, rect.rows),
                (*c, *rr),
                "layout size == derived size"
            );
        }
    }
}
