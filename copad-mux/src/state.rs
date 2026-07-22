//! The single authoritative actor (agent-mux-spec.md §1). Every mutation — from
//! a client OR the control API — is one `apply(Command)` call, applied serially
//! against `&mut State`. There is exactly one writer, so events are totally
//! ordered and the concurrency invariants (G1, I1, M1–M3) hold by construction.

use std::collections::HashMap;

use crate::model::{
    AgentState, ClientId, Dir, PaneId, Rect, Role, SplitTree, Tab, TabId, Terminal, TerminalId,
    Workspace, WorkspaceId,
};

/// Who issued a mutation. Client mutations require the client hold the control
/// lease (I1); API mutations are authorized separately (spec §6, not modeled
/// here) but still honor `if_rev` (M2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Client(ClientId),
    Api,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Attach a client to a workspace. `Controller` grants the lease only if
    /// vacant, or if `takeover` demotes the incumbent (spec §2). `cols`/`rows`
    /// are the client's own viewport; when the client becomes controller they
    /// become the workspace viewport that drives PTY geometry (G2).
    Attach {
        client: ClientId,
        workspace: WorkspaceId,
        role: Role,
        takeover: bool,
        cols: u16,
        rows: u16,
    },
    Detach {
        client: ClientId,
    },
    /// Controller-only: set the viewport, re-derive terminal geometry (G2/G3).
    Resize {
        client: ClientId,
        cols: u16,
        rows: u16,
    },
    /// Controller-only input (I1), routed to a target pane. The pure model
    /// validates the controller + that the pane is a valid routing target (exists
    /// in the active tab); bytes are not stored.
    Input {
        client: ClientId,
        pane: PaneId,
    },
    /// Controller-only: change the focused pane of the active tab.
    FocusPane {
        client: ClientId,
        pane: PaneId,
    },
    /// Split a pane. Client origin must be the controller; `if_rev` (if set)
    /// must equal the target tab's rev or the split is rejected as stale (M2).
    SplitPane {
        origin: Origin,
        workspace: WorkspaceId,
        pane: PaneId,
        dir: Dir,
        if_rev: Option<u64>,
    },
    /// Close a pane, collapsing its parent branch. Rejects closing a tab's last
    /// pane in this model.
    ClosePane {
        origin: Origin,
        workspace: WorkspaceId,
        pane: PaneId,
        if_rev: Option<u64>,
    },
    /// Create a new single-pane tab and make it active. Controller/API only.
    NewTab {
        origin: Origin,
        workspace: WorkspaceId,
    },
    /// Make `tab` the active tab; its geometry is re-derived while non-active tabs
    /// keep their frozen sizes. Controller/API only.
    SelectTab {
        origin: Origin,
        workspace: WorkspaceId,
        tab: TabId,
    },
    /// Close `tab` and drop all its terminals. Rejects closing a workspace's last
    /// tab. Controller/API only.
    CloseTab {
        origin: Origin,
        workspace: WorkspaceId,
        tab: TabId,
    },
}

/// A terminal's derived geometry + its new revision, carried in `Resized` so a
/// consumer observes both the size and the entity revision (M3).
#[derive(Debug, Clone, PartialEq)]
pub struct TermGeom {
    pub id: TerminalId,
    pub cols: u16,
    pub rows: u16,
    pub rev: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    ControlTransferred {
        workspace: WorkspaceId,
        from: Option<ClientId>,
        to: ClientId,
        workspace_rev: u64,
    },
    /// A workspace's control lease was released (controller detached, moved away,
    /// or demoted itself to observer) and the workspace is now uncontrolled. Emitted
    /// for the RELEASED workspace so the event stream stays complete (M3).
    ControlReleased {
        workspace: WorkspaceId,
        from: ClientId,
        workspace_rev: u64,
    },
    Attached {
        client: ClientId,
        workspace: WorkspaceId,
        role: Role,
    },
    Detached {
        client: ClientId,
    },
    Resized {
        workspace: WorkspaceId,
        cols: u16,
        rows: u16,
        terminals: Vec<TermGeom>,
        workspace_rev: u64,
    },
    Focused {
        workspace: WorkspaceId,
        tab: TabId,
        pane: PaneId,
        tab_rev: u64,
    },
    PaneSplit {
        workspace: WorkspaceId,
        tab: TabId,
        origin_pane: PaneId,
        new_pane: PaneId,
        new_terminal: TerminalId,
        dir: Dir,
        tab_rev: u64,
        workspace_rev: u64,
    },
    PaneClosed {
        workspace: WorkspaceId,
        tab: TabId,
        pane: PaneId,
        terminal: TerminalId,
        tab_rev: u64,
        workspace_rev: u64,
    },
    /// A new tab was created and made active. `terminal` is its lone pane's PTY,
    /// which the server must spawn.
    TabCreated {
        workspace: WorkspaceId,
        tab: TabId,
        pane: PaneId,
        terminal: TerminalId,
        workspace_rev: u64,
    },
    /// The active tab changed (geometry of the newly-active tab is re-derived and
    /// carried in the accompanying `Resized`).
    TabSelected {
        workspace: WorkspaceId,
        tab: TabId,
        workspace_rev: u64,
    },
    /// A tab was closed; `terminals` are its now-dead PTYs (the server must reap
    /// them), and `active` is the tab that became active in its place.
    TabClosed {
        workspace: WorkspaceId,
        tab: TabId,
        terminals: Vec<TerminalId>,
        active: TabId,
        workspace_rev: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MuxError {
    #[error("no such client")]
    NoSuchClient,
    #[error("no such workspace")]
    NoSuchWorkspace,
    #[error("no such pane")]
    NoSuchPane,
    #[error("client is not attached to that workspace")]
    NotAttached,
    #[error("client does not hold the control lease")]
    NotController,
    #[error("a controller already holds this workspace (attach as observer or takeover)")]
    ControllerExists,
    #[error("stale revision (current is {current})")]
    StaleRev { current: u64 },
    #[error("cannot close the last pane of a tab")]
    CannotCloseLastPane,
    #[error("no such tab")]
    NoSuchTab,
    #[error("cannot close the last tab of a workspace")]
    CannotCloseLastTab,
}

struct Attach {
    workspace: WorkspaceId,
    role: Role,
    /// The client's own viewport. A controller's viewport drives PTY geometry;
    /// an observer's is stored for its own client-side letterboxing (spec §2).
    viewport: Rect,
}

/// The authoritative multiplexer state. Not `Clone`/thread-shared: exactly one
/// task owns it and drives it via `apply` (spec §1).
pub struct State {
    workspaces: Vec<Workspace>,
    terminals: HashMap<TerminalId, Terminal>,
    clients: HashMap<ClientId, Attach>,
    next_pane: u64,
    next_term: u64,
    /// Monotonic tab counter for minting unique tab ids (`create_workspace` seeds
    /// `t0`, so new tabs start at `t1`).
    next_tab: u64,
}

impl State {
    pub fn new() -> Self {
        Self {
            workspaces: Vec::new(),
            terminals: HashMap::new(),
            clients: HashMap::new(),
            next_pane: 0,
            next_term: 0,
            next_tab: 1,
        }
    }

    // --- read helpers (for callers + tests) ---
    pub fn workspace(&self, id: &WorkspaceId) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| &w.id == id)
    }
    /// All workspace (session) ids in creation order.
    pub fn workspace_ids(&self) -> Vec<WorkspaceId> {
        self.workspaces.iter().map(|w| w.id.clone()).collect()
    }
    pub fn workspace_count(&self) -> usize {
        self.workspaces.len()
    }
    pub fn terminal(&self, id: &TerminalId) -> Option<&Terminal> {
        self.terminals.get(id)
    }
    pub fn controller_of(&self, id: &WorkspaceId) -> Option<ClientId> {
        self.workspace(id).and_then(|w| w.controller)
    }
    pub fn role_of(&self, client: ClientId) -> Option<Role> {
        self.clients.get(&client).map(|a| a.role)
    }

    /// Seed a workspace with one full-viewport terminal pane. Returns the ids so
    /// tests/servers can address them. (Server bootstrap; not a `Command`.)
    pub fn create_workspace(
        &mut self,
        id: WorkspaceId,
        name: Option<String>,
        viewport: Rect,
    ) -> (TabId, PaneId, TerminalId) {
        let tab_id = TabId::new(format!("{id}:t0"));
        let pane = self.mint_pane();
        let term = self.mint_terminal(viewport.cols, viewport.rows);
        let tab = Tab {
            id: tab_id.clone(),
            name: None,
            layout: SplitTree::Leaf {
                pane: pane.clone(),
                terminal: term.clone(),
            },
            focused: pane.clone(),
            rev: 0,
        };
        self.workspaces.push(Workspace {
            id: id.clone(),
            name,
            tabs: vec![tab],
            active_tab: tab_id.clone(),
            controller: None,
            viewport,
            rev: 0,
        });
        (tab_id, pane, term)
    }

    /// Remove a whole workspace (session) and drop all its terminals + any client
    /// attachments to it. Returns the removed terminal ids so the caller can reap
    /// their PTYs, or `None` if it is the only workspace (refused — a mux always has
    /// ≥1 session) or absent. (Server-level lifecycle; not a `Command`.)
    pub fn remove_workspace(&mut self, id: &WorkspaceId) -> Option<Vec<TerminalId>> {
        if self.workspaces.len() <= 1 {
            return None;
        }
        let pos = self.workspaces.iter().position(|w| &w.id == id)?;
        let ws = self.workspaces.remove(pos);
        let mut terms = Vec::new();
        for t in &ws.tabs {
            for p in t.layout.panes() {
                if let Some(tid) = t.layout.terminal_of(&p) {
                    self.terminals.remove(tid);
                    terms.push(tid.clone());
                }
            }
        }
        // Any client attached to the removed workspace is now detached.
        self.clients.retain(|_, a| a.workspace != *id);
        Some(terms)
    }

    fn mint_pane(&mut self) -> PaneId {
        let p = PaneId::new(format!("p{}", self.next_pane));
        self.next_pane += 1;
        p
    }
    fn mint_terminal(&mut self, cols: u16, rows: u16) -> TerminalId {
        let id = TerminalId::new(format!("term{}", self.next_term));
        self.next_term += 1;
        self.terminals.insert(
            id.clone(),
            Terminal {
                id: id.clone(),
                cols,
                rows,
                agent: AgentState::Unknown,
                rev: 0,
            },
        );
        id
    }

    fn ws_index(&self, id: &WorkspaceId) -> Result<usize, MuxError> {
        self.workspaces
            .iter()
            .position(|w| &w.id == id)
            .ok_or(MuxError::NoSuchWorkspace)
    }

    /// The one entry point. Serial by construction (`&mut self`).
    pub fn apply(&mut self, cmd: Command) -> Result<Vec<Event>, MuxError> {
        match cmd {
            Command::Attach {
                client,
                workspace,
                role,
                takeover,
                cols,
                rows,
            } => self.attach(client, workspace, role, takeover, cols, rows),
            Command::Detach { client } => self.detach(client),
            Command::Resize { client, cols, rows } => self.resize(client, cols, rows),
            Command::Input { client, pane } => {
                let ws_id = self.require_controller_of_attached(client)?;
                let idx = self.ws_index(&ws_id)?;
                // The routing target must be a live pane in the ACTIVE tab —
                // reject input to a nonexistent / closed / background / foreign pane.
                let active = self.workspaces[idx].active_tab.clone();
                let ok = self.workspaces[idx]
                    .tab(&active)
                    .map(|t| t.layout.contains(&pane))
                    .unwrap_or(false);
                if !ok {
                    return Err(MuxError::NoSuchPane);
                }
                Ok(vec![]) // accepted; bytes flow to the PTY outside the pure model
            }
            Command::FocusPane { client, pane } => self.focus(client, pane),
            Command::SplitPane {
                origin,
                workspace,
                pane,
                dir,
                if_rev,
            } => self.split(origin, workspace, pane, dir, if_rev),
            Command::ClosePane {
                origin,
                workspace,
                pane,
                if_rev,
            } => self.close(origin, workspace, pane, if_rev),
            Command::NewTab { origin, workspace } => self.new_tab(origin, workspace),
            Command::SelectTab {
                origin,
                workspace,
                tab,
            } => self.select_tab(origin, workspace, tab),
            Command::CloseTab {
                origin,
                workspace,
                tab,
            } => self.close_tab(origin, workspace, tab),
        }
    }

    // --- lease (spec §2) ---
    fn attach(
        &mut self,
        client: ClientId,
        workspace: WorkspaceId,
        role: Role,
        takeover: bool,
        cols: u16,
        rows: u16,
    ) -> Result<Vec<Event>, MuxError> {
        let idx = self.ws_index(&workspace)?;
        let vp = Rect {
            cols: cols.max(1),
            rows: rows.max(1),
        };

        // Idempotent controller re-attach of the same workspace: refresh the
        // viewport + reflow, keep the lease.
        if role == Role::Controller && self.workspaces[idx].controller == Some(client) {
            if let Some(a) = self.clients.get_mut(&client) {
                a.viewport = vp;
            }
            self.workspaces[idx].viewport = vp;
            self.workspaces[idx].rev += 1;
            let ws_rev = self.workspaces[idx].rev;
            let terms = self.recompute_sizes(idx);
            return Ok(vec![Event::Resized {
                workspace,
                cols: vp.cols,
                rows: vp.rows,
                terminals: terms,
                workspace_rev: ws_rev,
            }]);
        }

        // PRE-VALIDATE before any mutation (atomic transitions): reject an
        // occupied-controller attach *before* releasing this client's prior lease,
        // so a rejected command never leaves another workspace leaderless.
        if role == Role::Controller
            && let Some(existing) = self.workspaces[idx].controller
            && existing != client
            && !takeover
        {
            return Err(MuxError::ControllerExists);
        }

        // The attach is now guaranteed to succeed → release any prior lease this
        // client held elsewhere (moving workspaces / demoting to observer). The
        // release event (if any) is prepended so the stream stays complete (M3).
        let released = self.release_lease_if_held(client);

        if role == Role::Observer {
            self.clients.insert(
                client,
                Attach {
                    workspace: workspace.clone(),
                    role: Role::Observer,
                    viewport: vp,
                },
            );
            let mut evs = Vec::new();
            evs.extend(released);
            evs.push(Event::Attached {
                client,
                workspace,
                role: Role::Observer,
            });
            return Ok(evs);
        }

        // role == Controller, guaranteed grantable (workspace vacant, or takeover).
        let prev = self.workspaces[idx].controller;
        if let Some(existing) = prev {
            // takeover: demote the incumbent to observer (keeps its viewport).
            if let Some(a) = self.clients.get_mut(&existing) {
                a.role = Role::Observer;
            }
        }
        self.workspaces[idx].controller = Some(client);
        self.workspaces[idx].viewport = vp;
        self.workspaces[idx].rev += 1;
        let ws_rev = self.workspaces[idx].rev;
        self.clients.insert(
            client,
            Attach {
                workspace: workspace.clone(),
                role: Role::Controller,
                viewport: vp,
            },
        );
        // Reflow from the NEW controller's viewport (grant and takeover alike).
        let terms = self.recompute_sizes(idx);
        let mut evs = Vec::new();
        evs.extend(released);
        evs.push(Event::ControlTransferred {
            workspace: workspace.clone(),
            from: prev,
            to: client,
            workspace_rev: ws_rev,
        });
        evs.push(Event::Resized {
            workspace,
            cols: vp.cols,
            rows: vp.rows,
            terminals: terms,
            workspace_rev: ws_rev,
        });
        Ok(evs)
    }

    fn detach(&mut self, client: ClientId) -> Result<Vec<Event>, MuxError> {
        let Some(att) = self.clients.remove(&client) else {
            return Err(MuxError::NoSuchClient);
        };
        let mut evs = Vec::new();
        // Releasing the control lease leaves the workspace Uncontrolled; sizes
        // freeze at their last value (G3). Emit ControlReleased so the stream
        // stays complete (M3).
        if let Ok(idx) = self.ws_index(&att.workspace)
            && self.workspaces[idx].controller == Some(client)
        {
            self.workspaces[idx].controller = None;
            self.workspaces[idx].rev += 1;
            evs.push(Event::ControlReleased {
                workspace: att.workspace,
                from: client,
                workspace_rev: self.workspaces[idx].rev,
            });
        }
        evs.push(Event::Detached { client });
        Ok(evs)
    }

    /// If `client` currently holds the control lease of its attached workspace,
    /// release it (controller → None, bump rev) and return a `ControlReleased`
    /// event for that workspace. Returns `None` if the client held no lease.
    /// Idempotent; leaves client role/attach as-is otherwise.
    fn release_lease_if_held(&mut self, client: ClientId) -> Option<Event> {
        let att = self.clients.get(&client)?;
        let ws = att.workspace.clone();
        let i = self.ws_index(&ws).ok()?;
        if self.workspaces[i].controller == Some(client) {
            self.workspaces[i].controller = None;
            self.workspaces[i].rev += 1;
            Some(Event::ControlReleased {
                workspace: ws,
                from: client,
                workspace_rev: self.workspaces[i].rev,
            })
        } else {
            None
        }
    }

    // --- geometry (spec §2) ---
    fn resize(&mut self, client: ClientId, cols: u16, rows: u16) -> Result<Vec<Event>, MuxError> {
        let ws_id = self.require_controller_of_attached(client)?;
        let vp = Rect {
            cols: cols.max(1),
            rows: rows.max(1),
        };
        if let Some(a) = self.clients.get_mut(&client) {
            a.viewport = vp;
        }
        let idx = self.ws_index(&ws_id)?;
        self.workspaces[idx].viewport = vp;
        self.workspaces[idx].rev += 1;
        let ws_rev = self.workspaces[idx].rev;
        let terms = self.recompute_sizes(idx);
        Ok(vec![Event::Resized {
            workspace: ws_id,
            cols: cols.max(1),
            rows: rows.max(1),
            terminals: terms,
            workspace_rev: ws_rev,
        }])
    }

    /// Re-derive the active tab's terminal sizes from the workspace viewport and
    /// write them back (G2), returning each terminal's new geometry + revision.
    /// Non-active tabs keep their last sizes. Called on every viewport change
    /// (controller resize) AND every active-tab layout change (split/close) — a
    /// terminal's size therefore always equals its derived share of
    /// (viewport, layout).
    fn recompute_sizes(&mut self, idx: usize) -> Vec<TermGeom> {
        let ws = &self.workspaces[idx];
        let area = ws.viewport;
        let Some(tab) = ws.tab(&ws.active_tab) else {
            return vec![];
        };
        let mut derived = Vec::new();
        tab.layout.derive_sizes(area, &mut derived);
        let mut out = Vec::with_capacity(derived.len());
        for (tid, c, r) in &derived {
            if let Some(t) = self.terminals.get_mut(tid) {
                if t.cols != *c || t.rows != *r {
                    t.cols = *c;
                    t.rows = *r;
                    t.rev += 1;
                }
                out.push(TermGeom {
                    id: tid.clone(),
                    cols: t.cols,
                    rows: t.rows,
                    rev: t.rev,
                });
            }
        }
        out
    }

    // --- focus (controller-only) ---
    fn focus(&mut self, client: ClientId, pane: PaneId) -> Result<Vec<Event>, MuxError> {
        let ws_id = self.require_controller_of_attached(client)?;
        let idx = self.ws_index(&ws_id)?;
        let active = self.workspaces[idx].active_tab.clone();
        let Some(tab) = self.workspaces[idx].tab_mut(&active) else {
            return Err(MuxError::NoSuchPane);
        };
        if !tab.layout.contains(&pane) {
            return Err(MuxError::NoSuchPane);
        }
        tab.focused = pane.clone();
        tab.rev += 1;
        let tab_rev = tab.rev;
        Ok(vec![Event::Focused {
            workspace: ws_id,
            tab: active,
            pane,
            tab_rev,
        }])
    }

    // --- mutations (spec §4a) ---
    fn authorize_mutation(&self, origin: &Origin, ws: &WorkspaceId) -> Result<(), MuxError> {
        match origin {
            Origin::Api => Ok(()),
            Origin::Client(c) => {
                let att = self.clients.get(c).ok_or(MuxError::NoSuchClient)?;
                if &att.workspace != ws {
                    return Err(MuxError::NotAttached);
                }
                if att.role != Role::Controller {
                    return Err(MuxError::NotController);
                }
                Ok(())
            }
        }
    }

    fn split(
        &mut self,
        origin: Origin,
        workspace: WorkspaceId,
        pane: PaneId,
        dir: Dir,
        if_rev: Option<u64>,
    ) -> Result<Vec<Event>, MuxError> {
        self.authorize_mutation(&origin, &workspace)?;
        let idx = self.ws_index(&workspace)?;
        let tab_id = self.workspaces[idx]
            .tab_of_pane(&pane)
            .map(|t| t.id.clone())
            .ok_or(MuxError::NoSuchPane)?;

        // M2: optimistic concurrency against the tab rev.
        let cur_rev = self.workspaces[idx].tab(&tab_id).unwrap().rev;
        if let Some(exp) = if_rev
            && exp != cur_rev
        {
            return Err(MuxError::StaleRev { current: cur_rev });
        }

        let new_pane = self.mint_pane();
        let new_term = self.mint_terminal(1, 1); // real size assigned by recompute
        let tab = self.workspaces[idx].tab_mut(&tab_id).unwrap();
        let ok = tab
            .layout
            .split_leaf(&pane, dir, new_pane.clone(), new_term.clone());
        debug_assert!(ok, "pane was located but split_leaf missed it");
        tab.rev += 1;
        let tab_rev = tab.rev;
        self.workspaces[idx].rev += 1;
        let ws_rev = self.workspaces[idx].rev;

        let mut evs = vec![Event::PaneSplit {
            workspace: workspace.clone(),
            tab: tab_id.clone(),
            origin_pane: pane,
            new_pane,
            new_terminal: new_term,
            dir,
            tab_rev,
            workspace_rev: ws_rev,
        }];
        // A layout change redistributes terminal sizes within the current
        // viewport; emit the reflow so the geometry change is observable (M3).
        if self.workspaces[idx].active_tab == tab_id {
            let terms = self.recompute_sizes(idx);
            let vp = self.workspaces[idx].viewport;
            evs.push(Event::Resized {
                workspace,
                cols: vp.cols,
                rows: vp.rows,
                terminals: terms,
                workspace_rev: ws_rev,
            });
        }
        Ok(evs)
    }

    fn close(
        &mut self,
        origin: Origin,
        workspace: WorkspaceId,
        pane: PaneId,
        if_rev: Option<u64>,
    ) -> Result<Vec<Event>, MuxError> {
        self.authorize_mutation(&origin, &workspace)?;
        let idx = self.ws_index(&workspace)?;
        let tab_id = self.workspaces[idx]
            .tab_of_pane(&pane)
            .map(|t| t.id.clone())
            .ok_or(MuxError::NoSuchPane)?;

        let cur_rev = self.workspaces[idx].tab(&tab_id).unwrap().rev;
        if let Some(exp) = if_rev
            && exp != cur_rev
        {
            return Err(MuxError::StaleRev { current: cur_rev });
        }
        if self.workspaces[idx]
            .tab(&tab_id)
            .unwrap()
            .layout
            .is_single_leaf()
        {
            return Err(MuxError::CannotCloseLastPane);
        }

        let tab = self.workspaces[idx].tab_mut(&tab_id).unwrap();
        let term = tab.layout.remove_leaf(&pane).ok_or(MuxError::NoSuchPane)?;
        // Refocus if we closed the focused pane.
        if tab.focused == pane {
            tab.focused = tab.layout.leftmost_pane();
        }
        tab.rev += 1;
        let tab_rev = tab.rev;
        self.terminals.remove(&term);
        self.workspaces[idx].rev += 1;
        let ws_rev = self.workspaces[idx].rev;

        let mut evs = vec![Event::PaneClosed {
            workspace: workspace.clone(),
            tab: tab_id.clone(),
            pane,
            terminal: term,
            tab_rev,
            workspace_rev: ws_rev,
        }];
        if self.workspaces[idx].active_tab == tab_id {
            let terms = self.recompute_sizes(idx);
            let vp = self.workspaces[idx].viewport;
            evs.push(Event::Resized {
                workspace,
                cols: vp.cols,
                rows: vp.rows,
                terminals: terms,
                workspace_rev: ws_rev,
            });
        }
        Ok(evs)
    }

    // --- tabs (spec §1: multiple layouts per workspace) ---

    fn new_tab(&mut self, origin: Origin, workspace: WorkspaceId) -> Result<Vec<Event>, MuxError> {
        self.authorize_mutation(&origin, &workspace)?;
        let idx = self.ws_index(&workspace)?;
        let n = self.next_tab;
        self.next_tab += 1;
        let tab_id = TabId::new(format!("{workspace}:t{n}"));
        let pane = self.mint_pane();
        let vp = self.workspaces[idx].viewport;
        let term = self.mint_terminal(vp.cols, vp.rows);
        let tab = Tab {
            id: tab_id.clone(),
            name: None,
            layout: SplitTree::Leaf {
                pane: pane.clone(),
                terminal: term.clone(),
            },
            focused: pane.clone(),
            rev: 0,
        };
        self.workspaces[idx].tabs.push(tab);
        self.workspaces[idx].active_tab = tab_id.clone();
        self.workspaces[idx].rev += 1;
        let ws_rev = self.workspaces[idx].rev;
        let terms = self.recompute_sizes(idx);
        let vp = self.workspaces[idx].viewport;
        Ok(vec![
            Event::TabCreated {
                workspace: workspace.clone(),
                tab: tab_id,
                pane,
                terminal: term,
                workspace_rev: ws_rev,
            },
            Event::Resized {
                workspace,
                cols: vp.cols,
                rows: vp.rows,
                terminals: terms,
                workspace_rev: ws_rev,
            },
        ])
    }

    fn select_tab(
        &mut self,
        origin: Origin,
        workspace: WorkspaceId,
        tab: TabId,
    ) -> Result<Vec<Event>, MuxError> {
        self.authorize_mutation(&origin, &workspace)?;
        let idx = self.ws_index(&workspace)?;
        if self.workspaces[idx].tab(&tab).is_none() {
            return Err(MuxError::NoSuchTab);
        }
        // Selecting the already-active tab is a no-op (no rev bump, no events).
        if self.workspaces[idx].active_tab == tab {
            return Ok(vec![]);
        }
        self.workspaces[idx].active_tab = tab.clone();
        self.workspaces[idx].rev += 1;
        let ws_rev = self.workspaces[idx].rev;
        // Re-derive the newly-active tab's geometry from the (unchanged) viewport,
        // so a tab created/reflowed under a different size catches up (G2).
        let terms = self.recompute_sizes(idx);
        let vp = self.workspaces[idx].viewport;
        Ok(vec![
            Event::TabSelected {
                workspace: workspace.clone(),
                tab,
                workspace_rev: ws_rev,
            },
            Event::Resized {
                workspace,
                cols: vp.cols,
                rows: vp.rows,
                terminals: terms,
                workspace_rev: ws_rev,
            },
        ])
    }

    fn close_tab(
        &mut self,
        origin: Origin,
        workspace: WorkspaceId,
        tab: TabId,
    ) -> Result<Vec<Event>, MuxError> {
        self.authorize_mutation(&origin, &workspace)?;
        let idx = self.ws_index(&workspace)?;
        let pos = self.workspaces[idx]
            .tabs
            .iter()
            .position(|t| t.id == tab)
            .ok_or(MuxError::NoSuchTab)?;
        if self.workspaces[idx].tabs.len() == 1 {
            return Err(MuxError::CannotCloseLastTab);
        }
        // Collect + drop the tab's terminals so no orphan runtime survives it.
        let terminals: Vec<TerminalId> = {
            let t = &self.workspaces[idx].tabs[pos];
            t.layout
                .panes()
                .iter()
                .filter_map(|p| t.layout.terminal_of(p).cloned())
                .collect()
        };
        for term in &terminals {
            self.terminals.remove(term);
        }
        let removed_active = self.workspaces[idx].active_tab == tab;
        self.workspaces[idx].tabs.remove(pos);
        // If we closed the active tab, fall to the neighbour (prefer the tab that
        // shifted into this slot, else the new last tab).
        if removed_active {
            let new_pos = pos.min(self.workspaces[idx].tabs.len() - 1);
            self.workspaces[idx].active_tab = self.workspaces[idx].tabs[new_pos].id.clone();
        }
        self.workspaces[idx].rev += 1;
        let ws_rev = self.workspaces[idx].rev;
        let active = self.workspaces[idx].active_tab.clone();
        let terms = self.recompute_sizes(idx);
        let vp = self.workspaces[idx].viewport;
        Ok(vec![
            Event::TabClosed {
                workspace: workspace.clone(),
                tab,
                terminals,
                active,
                workspace_rev: ws_rev,
            },
            Event::Resized {
                workspace,
                cols: vp.cols,
                rows: vp.rows,
                terminals: terms,
                workspace_rev: ws_rev,
            },
        ])
    }

    fn require_controller_of_attached(&self, client: ClientId) -> Result<WorkspaceId, MuxError> {
        let att = self.clients.get(&client).ok_or(MuxError::NoSuchClient)?;
        if att.role != Role::Controller {
            return Err(MuxError::NotController);
        }
        // Belt-and-braces: the workspace must still name this client.
        if self.controller_of(&att.workspace) != Some(client) {
            return Err(MuxError::NotController);
        }
        Ok(att.workspace.clone())
    }

    // --- invariant checks (used by tests; cheap enough to expose) ---

    /// Invariant G1: at most one controller per workspace, and the workspace's
    /// named controller is attached with `Role::Controller`.
    pub fn check_g1(&self) -> bool {
        for w in &self.workspaces {
            match w.controller {
                None => {
                    // No client may claim controller of a leaseless workspace.
                    if self
                        .clients
                        .iter()
                        .any(|(_, a)| a.workspace == w.id && a.role == Role::Controller)
                    {
                        return false;
                    }
                }
                Some(c) => {
                    let controllers: Vec<_> = self
                        .clients
                        .iter()
                        .filter(|(_, a)| a.workspace == w.id && a.role == Role::Controller)
                        .map(|(id, _)| *id)
                        .collect();
                    if controllers != vec![c] {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Structural invariant: every leaf terminal exists in the terminal table
    /// and no orphan terminals remain.
    pub fn check_tree_consistency(&self) -> bool {
        let mut referenced = std::collections::HashSet::new();
        for w in &self.workspaces {
            for t in &w.tabs {
                for p in t.layout.panes() {
                    match t.layout.terminal_of(&p) {
                        Some(term) if self.terminals.contains_key(term) => {
                            referenced.insert(term.clone());
                        }
                        _ => return false,
                    }
                }
                // focused pane must exist
                if !t.layout.contains(&t.focused) {
                    return false;
                }
            }
            // active tab must exist
            if w.tab(&w.active_tab).is_none() {
                return false;
            }
        }
        referenced.len() == self.terminals.len()
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Rect;

    fn seed() -> (State, WorkspaceId, PaneId) {
        let mut s = State::new();
        let ws = WorkspaceId::new("w1");
        let (_tab, pane, _term) = s.create_workspace(ws.clone(), None, Rect { cols: 80, rows: 24 });
        (s, ws, pane)
    }

    #[test]
    fn first_controller_gets_lease_and_sizes() {
        let (mut s, ws, _p) = seed();
        let c = ClientId(1);
        let ev = s
            .apply(Command::Attach {
                client: c,
                workspace: ws.clone(),
                role: Role::Controller,
                takeover: false,
                cols: 80,
                rows: 24,
            })
            .unwrap();
        assert_eq!(s.controller_of(&ws), Some(c));
        assert!(matches!(
            ev[0],
            Event::ControlTransferred { from: None, .. }
        ));
        assert!(s.check_g1());
    }

    #[test]
    fn second_controller_denied_without_takeover() {
        let (mut s, ws, _p) = seed();
        let (a, b) = (ClientId(1), ClientId(2));
        s.apply(Command::Attach {
            client: a,
            workspace: ws.clone(),
            role: Role::Controller,
            takeover: false,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        let err = s
            .apply(Command::Attach {
                client: b,
                workspace: ws.clone(),
                role: Role::Controller,
                takeover: false,
                cols: 80,
                rows: 24,
            })
            .unwrap_err();
        assert_eq!(err, MuxError::ControllerExists);
        assert_eq!(s.controller_of(&ws), Some(a));
        assert!(s.check_g1());
    }

    #[test]
    fn takeover_transfers_lease_and_demotes_incumbent() {
        let (mut s, ws, _p) = seed();
        let (a, b) = (ClientId(1), ClientId(2));
        s.apply(Command::Attach {
            client: a,
            workspace: ws.clone(),
            role: Role::Controller,
            takeover: false,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        let ev = s
            .apply(Command::Attach {
                client: b,
                workspace: ws.clone(),
                role: Role::Controller,
                takeover: true,
                cols: 80,
                rows: 24,
            })
            .unwrap();
        assert_eq!(s.controller_of(&ws), Some(b));
        assert_eq!(s.role_of(a), Some(Role::Observer));
        assert!(
            matches!(ev[0], Event::ControlTransferred { from: Some(x), to, .. } if x == a && to == b)
        );
        assert!(s.check_g1());
    }

    #[test]
    fn non_controller_cannot_resize_or_mutate() {
        let (mut s, ws, pane) = seed();
        let (a, obs) = (ClientId(1), ClientId(2));
        s.apply(Command::Attach {
            client: a,
            workspace: ws.clone(),
            role: Role::Controller,
            takeover: false,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        s.apply(Command::Attach {
            client: obs,
            workspace: ws.clone(),
            role: Role::Observer,
            takeover: false,
            cols: 80,
            rows: 24,
        })
        .unwrap();

        assert_eq!(
            s.apply(Command::Resize {
                client: obs,
                cols: 100,
                rows: 40
            })
            .unwrap_err(),
            MuxError::NotController
        );
        assert_eq!(
            s.apply(Command::SplitPane {
                origin: Origin::Client(obs),
                workspace: ws.clone(),
                pane: pane.clone(),
                dir: Dir::Right,
                if_rev: None
            })
            .unwrap_err(),
            MuxError::NotController
        );
    }

    #[test]
    fn controller_resize_rederives_geometry() {
        let (mut s, ws, pane) = seed();
        let c = ClientId(1);
        s.apply(Command::Attach {
            client: c,
            workspace: ws.clone(),
            role: Role::Controller,
            takeover: false,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        // split so there are two terminals sharing the width
        s.apply(Command::SplitPane {
            origin: Origin::Client(c),
            workspace: ws.clone(),
            pane: pane.clone(),
            dir: Dir::Right,
            if_rev: None,
        })
        .unwrap();
        s.apply(Command::Resize {
            client: c,
            cols: 100,
            rows: 30,
        })
        .unwrap();
        // two terminals, each roughly half of (100-1) cols, full 30 rows
        let sizes: Vec<_> = {
            let w = s.workspace(&ws).unwrap();
            let tab = w.tab(&w.active_tab).unwrap();
            tab.layout
                .panes()
                .iter()
                .map(|p| {
                    let t = tab.layout.terminal_of(p).unwrap();
                    let term = s.terminal(t).unwrap();
                    (term.cols, term.rows)
                })
                .collect()
        };
        assert_eq!(sizes.len(), 2);
        assert!(sizes.iter().all(|(_, r)| *r == 30));
        assert_eq!(sizes.iter().map(|(c, _)| *c as u32).sum::<u32>(), 99); // 49 + 50, divider = 1
    }

    #[test]
    fn stale_if_rev_is_rejected() {
        let (mut s, ws, pane) = seed();
        let c = ClientId(1);
        s.apply(Command::Attach {
            client: c,
            workspace: ws.clone(),
            role: Role::Controller,
            takeover: false,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        // tab rev starts at 0; split with correct if_rev succeeds and bumps to 1
        s.apply(Command::SplitPane {
            origin: Origin::Api,
            workspace: ws.clone(),
            pane: pane.clone(),
            dir: Dir::Down,
            if_rev: Some(0),
        })
        .unwrap();
        // a second split with the now-stale if_rev=0 must be rejected
        let err = s
            .apply(Command::SplitPane {
                origin: Origin::Api,
                workspace: ws.clone(),
                pane: pane.clone(),
                dir: Dir::Down,
                if_rev: Some(0),
            })
            .unwrap_err();
        assert_eq!(err, MuxError::StaleRev { current: 1 });
    }

    #[test]
    fn split_then_close_collapses_and_refocuses() {
        let (mut s, ws, pane) = seed();
        let c = ClientId(1);
        s.apply(Command::Attach {
            client: c,
            workspace: ws.clone(),
            role: Role::Controller,
            takeover: false,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        let ev = s
            .apply(Command::SplitPane {
                origin: Origin::Client(c),
                workspace: ws.clone(),
                pane: pane.clone(),
                dir: Dir::Right,
                if_rev: None,
            })
            .unwrap();
        let Event::PaneSplit { new_pane, .. } = &ev[0] else {
            panic!()
        };
        let new_pane = new_pane.clone();
        assert!(s.check_tree_consistency());
        // close the original; tree collapses back to a single leaf (the new pane)
        s.apply(Command::ClosePane {
            origin: Origin::Client(c),
            workspace: ws.clone(),
            pane: pane.clone(),
            if_rev: None,
        })
        .unwrap();
        let w = s.workspace(&ws).unwrap();
        let tab = w.tab(&w.active_tab).unwrap();
        assert!(tab.layout.is_single_leaf());
        assert_eq!(tab.focused, new_pane);
        assert!(s.check_tree_consistency());
    }

    #[test]
    fn cannot_close_last_pane() {
        let (mut s, ws, pane) = seed();
        let c = ClientId(1);
        s.apply(Command::Attach {
            client: c,
            workspace: ws.clone(),
            role: Role::Controller,
            takeover: false,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        assert_eq!(
            s.apply(Command::ClosePane {
                origin: Origin::Client(c),
                workspace: ws.clone(),
                pane,
                if_rev: None
            })
            .unwrap_err(),
            MuxError::CannotCloseLastPane
        );
    }

    #[test]
    fn takeover_reflows_from_new_controller_viewport() {
        let (mut s, ws, pane) = seed();
        let a = ClientId(1);
        s.apply(Command::Attach {
            client: a,
            workspace: ws.clone(),
            role: Role::Controller,
            takeover: false,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        s.apply(Command::SplitPane {
            origin: Origin::Client(a),
            workspace: ws.clone(),
            pane,
            dir: Dir::Right,
            if_rev: None,
        })
        .unwrap();
        let b = ClientId(2);
        let ev = s
            .apply(Command::Attach {
                client: b,
                workspace: ws.clone(),
                role: Role::Controller,
                takeover: true,
                cols: 120,
                rows: 40,
            })
            .unwrap();
        assert!(
            ev.iter().any(|e| matches!(
                e,
                Event::Resized {
                    cols: 120,
                    rows: 40,
                    ..
                }
            )),
            "takeover must reflow to the new controller's viewport"
        );
        let w = s.workspace(&ws).unwrap();
        assert_eq!(
            w.viewport,
            Rect {
                cols: 120,
                rows: 40
            }
        );
        let tab = w.tab(&w.active_tab).unwrap();
        assert_eq!(
            tab.layout.footprint(w.viewport),
            w.viewport,
            "geometry must conserve the viewport (G2)"
        );
    }

    #[test]
    fn rejected_controller_attach_does_not_release_prior_lease() {
        // C's control of A must survive a rejected attempt to control an occupied B (atomicity).
        let mut s = State::new();
        let a = WorkspaceId::new("A");
        let b = WorkspaceId::new("B");
        s.create_workspace(a.clone(), None, Rect { cols: 80, rows: 24 });
        s.create_workspace(b.clone(), None, Rect { cols: 80, rows: 24 });
        let (c, other) = (ClientId(1), ClientId(2));
        s.apply(Command::Attach {
            client: c,
            workspace: a.clone(),
            role: Role::Controller,
            takeover: false,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        s.apply(Command::Attach {
            client: other,
            workspace: b.clone(),
            role: Role::Controller,
            takeover: false,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        let rev_before = s.workspace(&a).unwrap().rev;
        let err = s
            .apply(Command::Attach {
                client: c,
                workspace: b.clone(),
                role: Role::Controller,
                takeover: false,
                cols: 80,
                rows: 24,
            })
            .unwrap_err();
        assert_eq!(err, MuxError::ControllerExists);
        assert_eq!(
            s.controller_of(&a),
            Some(c),
            "A must still be controlled by c"
        );
        assert_eq!(
            s.workspace(&a).unwrap().rev,
            rev_before,
            "a rejected command must not mutate A"
        );
        assert!(s.check_g1());
    }

    #[test]
    fn detach_controller_emits_control_released() {
        let (mut s, ws, _p) = seed();
        let c = ClientId(1);
        s.apply(Command::Attach {
            client: c,
            workspace: ws.clone(),
            role: Role::Controller,
            takeover: false,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        let ev = s.apply(Command::Detach { client: c }).unwrap();
        assert!(
            ev.iter()
                .any(|e| matches!(e, Event::ControlReleased { from, .. } if *from == c)),
            "detaching a controller must emit ControlReleased"
        );
        assert!(ev.iter().any(|e| matches!(e, Event::Detached { .. })));
    }

    #[test]
    fn controller_reattaching_as_observer_releases_lease_with_event() {
        let (mut s, ws, _p) = seed();
        let c = ClientId(1);
        s.apply(Command::Attach {
            client: c,
            workspace: ws.clone(),
            role: Role::Controller,
            takeover: false,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        let ev = s
            .apply(Command::Attach {
                client: c,
                workspace: ws.clone(),
                role: Role::Observer,
                takeover: false,
                cols: 80,
                rows: 24,
            })
            .unwrap();
        assert_eq!(s.controller_of(&ws), None);
        assert!(
            ev.iter()
                .any(|e| matches!(e, Event::ControlReleased { .. })),
            "self-demotion must emit ControlReleased"
        );
        assert!(s.check_g1());
    }

    #[test]
    fn input_to_invalid_pane_is_rejected() {
        let (mut s, ws, _p) = seed();
        let c = ClientId(1);
        s.apply(Command::Attach {
            client: c,
            workspace: ws.clone(),
            role: Role::Controller,
            takeover: false,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        let err = s
            .apply(Command::Input {
                client: c,
                pane: PaneId::new("does-not-exist"),
            })
            .unwrap_err();
        assert_eq!(
            err,
            MuxError::NoSuchPane,
            "input to a nonexistent pane must be rejected"
        );
    }

    #[test]
    fn detach_releases_lease_and_freezes() {
        let (mut s, ws, _p) = seed();
        let c = ClientId(1);
        s.apply(Command::Attach {
            client: c,
            workspace: ws.clone(),
            role: Role::Controller,
            takeover: false,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        s.apply(Command::Detach { client: c }).unwrap();
        assert_eq!(s.controller_of(&ws), None);
        // a detached client can no longer resize (no controller ⇒ frozen, G3)
        assert_eq!(
            s.apply(Command::Resize {
                client: c,
                cols: 10,
                rows: 10
            })
            .unwrap_err(),
            MuxError::NoSuchClient
        );
        assert!(s.check_g1());
    }

    #[test]
    fn new_tab_becomes_active_with_a_fresh_single_pane() {
        let (mut s, ws, _p) = seed();
        let c = ClientId(1);
        s.apply(Command::Attach {
            client: c,
            workspace: ws.clone(),
            role: Role::Controller,
            takeover: false,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        let ev = s
            .apply(Command::NewTab {
                origin: Origin::Client(c),
                workspace: ws.clone(),
            })
            .unwrap();
        let Event::TabCreated { tab, .. } = &ev[0] else {
            panic!("first event must be TabCreated")
        };
        let w = s.workspace(&ws).unwrap();
        assert_eq!(w.tabs.len(), 2, "a second tab exists");
        assert_eq!(&w.active_tab, tab, "the new tab is active");
        assert!(
            w.tab(tab).unwrap().layout.is_single_leaf(),
            "a fresh tab has exactly one pane"
        );
        assert!(s.check_tree_consistency());
    }

    #[test]
    fn select_tab_switches_active_and_is_noop_when_already_active() {
        let (mut s, ws, _p) = seed();
        let c = ClientId(1);
        s.apply(Command::Attach {
            client: c,
            workspace: ws.clone(),
            role: Role::Controller,
            takeover: false,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        let first = s.workspace(&ws).unwrap().active_tab.clone();
        s.apply(Command::NewTab {
            origin: Origin::Client(c),
            workspace: ws.clone(),
        })
        .unwrap();
        // switch back to the original tab
        let ev = s
            .apply(Command::SelectTab {
                origin: Origin::Client(c),
                workspace: ws.clone(),
                tab: first.clone(),
            })
            .unwrap();
        assert_eq!(s.workspace(&ws).unwrap().active_tab, first);
        assert!(matches!(ev[0], Event::TabSelected { .. }));
        // re-selecting the active tab is a no-op (empty event stream, no rev bump)
        let rev = s.workspace(&ws).unwrap().rev;
        let ev = s
            .apply(Command::SelectTab {
                origin: Origin::Client(c),
                workspace: ws.clone(),
                tab: first,
            })
            .unwrap();
        assert!(ev.is_empty());
        assert_eq!(s.workspace(&ws).unwrap().rev, rev);
    }

    #[test]
    fn close_tab_drops_terminals_and_reassigns_active() {
        let (mut s, ws, _p) = seed();
        let c = ClientId(1);
        s.apply(Command::Attach {
            client: c,
            workspace: ws.clone(),
            role: Role::Controller,
            takeover: false,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        let ev = s
            .apply(Command::NewTab {
                origin: Origin::Client(c),
                workspace: ws.clone(),
            })
            .unwrap();
        let Event::TabCreated { tab, terminal, .. } = &ev[0] else {
            panic!()
        };
        let (new_tab, new_term) = (tab.clone(), terminal.clone());
        assert!(s.terminal(&new_term).is_some());
        let ev = s
            .apply(Command::CloseTab {
                origin: Origin::Client(c),
                workspace: ws.clone(),
                tab: new_tab.clone(),
            })
            .unwrap();
        assert!(
            matches!(&ev[0], Event::TabClosed { terminals, .. } if terminals.contains(&new_term))
        );
        let w = s.workspace(&ws).unwrap();
        assert_eq!(w.tabs.len(), 1, "back to a single tab");
        assert_ne!(w.active_tab, new_tab, "active moved off the closed tab");
        assert!(s.terminal(&new_term).is_none(), "the tab's PTY is dropped");
        assert!(s.check_tree_consistency());
    }

    #[test]
    fn remove_workspace_drops_terminals_and_refuses_the_last() {
        let (mut s, ws, _p) = seed();
        // a second session
        let ws2 = WorkspaceId::new("w2");
        let (_t, _p2, term2) = s.create_workspace(ws2.clone(), None, Rect { cols: 80, rows: 24 });
        assert_eq!(s.workspace_count(), 2);
        assert!(s.terminal(&term2).is_some());

        let removed = s
            .remove_workspace(&ws2)
            .expect("second workspace removable");
        assert!(removed.contains(&term2));
        assert!(s.terminal(&term2).is_none(), "its PTY is dropped");
        assert_eq!(s.workspace_count(), 1);
        assert!(s.workspace(&ws2).is_none());
        assert!(s.check_tree_consistency());

        // the last remaining workspace cannot be removed
        assert!(s.remove_workspace(&ws).is_none());
        assert_eq!(s.workspace_count(), 1);
    }

    #[test]
    fn cannot_close_the_last_tab() {
        let (mut s, ws, _p) = seed();
        let c = ClientId(1);
        s.apply(Command::Attach {
            client: c,
            workspace: ws.clone(),
            role: Role::Controller,
            takeover: false,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        let only = s.workspace(&ws).unwrap().active_tab.clone();
        assert_eq!(
            s.apply(Command::CloseTab {
                origin: Origin::Client(c),
                workspace: ws.clone(),
                tab: only,
            })
            .unwrap_err(),
            MuxError::CannotCloseLastTab
        );
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use crate::model::Rect;
    use proptest::prelude::*;

    /// A small operation universe over a fixed 3-client, 1-workspace world.
    #[derive(Debug, Clone)]
    enum Op {
        Attach(u64, bool, bool, u16, u16), // client, controller?, takeover?, cols, rows
        Detach(u64),
        Resize(u64, u16, u16),
        Input(u64),
        SplitApi(usize, bool), // pane index, dir=right?
        SplitClient(u64, usize, bool),
        CloseApi(usize),
        Focus(u64, usize),
        NewTab,
        SelectTab(usize),
        CloseTab(usize),
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0u64..3, any::<bool>(), any::<bool>(), 4u16..200, 4u16..80)
                .prop_map(|(c, ctl, t, w, h)| Op::Attach(c, ctl, t, w, h)),
            (0u64..3).prop_map(Op::Detach),
            (0u64..3, 4u16..200, 4u16..80).prop_map(|(c, w, h)| Op::Resize(c, w, h)),
            (0u64..3).prop_map(Op::Input),
            (0usize..6, any::<bool>()).prop_map(|(p, d)| Op::SplitApi(p, d)),
            (0u64..3, 0usize..6, any::<bool>()).prop_map(|(c, p, d)| Op::SplitClient(c, p, d)),
            (0usize..6).prop_map(Op::CloseApi),
            (0u64..3, 0usize..6).prop_map(|(c, p)| Op::Focus(c, p)),
            Just(Op::NewTab),
            (0usize..6).prop_map(Op::SelectTab),
            (0usize..6).prop_map(Op::CloseTab),
        ]
    }

    fn panes(s: &State, ws: &WorkspaceId) -> Vec<PaneId> {
        let w = s.workspace(ws).unwrap();
        w.tabs.iter().flat_map(|t| t.layout.panes()).collect()
    }

    fn tab_ids(s: &State, ws: &WorkspaceId) -> Vec<TabId> {
        s.workspace(ws)
            .unwrap()
            .tabs
            .iter()
            .map(|t| t.id.clone())
            .collect()
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 500, ..ProptestConfig::default() })]

        /// No matter how attach/detach/takeover/resize/split/close/focus interleave,
        /// the invariants hold after EVERY step:
        ///   G1   — ≤1 controller per workspace, consistent with client roles
        ///   G2   — the active tab's geometry conserves the viewport (footprint == viewport)
        ///   tree — every leaf terminal exists, no orphans, focus valid
        ///   rev  — workspace rev + every terminal rev never decrease
        ///   I1   — a SUCCEEDED client mutation / resize / focus implies controllership
        #[test]
        fn invariants_hold_under_interleaving(ops in proptest::collection::vec(op_strategy(), 0..60)) {
            use std::collections::HashMap;
            let mut s = State::new();
            let ws = WorkspaceId::new("w1");
            s.create_workspace(ws.clone(), None, Rect { cols: 80, rows: 24 });
            let mut last_ws_rev = s.workspace(&ws).unwrap().rev;
            let mut term_rev: HashMap<TerminalId, u64> = HashMap::new();

            for op in ops {
                let ps = panes(&s, &ws);
                let pick = |i: usize| ps.get(i % ps.len().max(1)).cloned();
                let is_ctl = |s: &State, c: u64| s.controller_of(&ws) == Some(ClientId(c));

                match op {
                    Op::Attach(c, ctl, t, w, h) => {
                        let _ = s.apply(Command::Attach {
                            client: ClientId(c), workspace: ws.clone(),
                            role: if ctl { Role::Controller } else { Role::Observer },
                            takeover: t, cols: w, rows: h,
                        });
                    }
                    Op::Detach(c) => { let _ = s.apply(Command::Detach { client: ClientId(c) }); }
                    Op::Resize(c, w, h) => {
                        let ctl_before = is_ctl(&s, c);
                        let r = s.apply(Command::Resize { client: ClientId(c), cols: w, rows: h });
                        if r.is_ok() { prop_assert!(ctl_before, "I1: resize succeeded from a non-controller"); }
                    }
                    Op::Input(c) => {
                        let p = pick(0).unwrap();
                        let ctl_before = is_ctl(&s, c);
                        let r = s.apply(Command::Input { client: ClientId(c), pane: p });
                        if r.is_ok() { prop_assert!(ctl_before, "I1: input accepted from a non-controller"); }
                    }
                    Op::SplitApi(pi, d) => {
                        let Some(p) = pick(pi) else { continue };
                        let _ = s.apply(Command::SplitPane { origin: Origin::Api, workspace: ws.clone(), pane: p, dir: if d { Dir::Right } else { Dir::Down }, if_rev: None });
                    }
                    Op::SplitClient(c, pi, d) => {
                        let Some(p) = pick(pi) else { continue };
                        let ctl_before = is_ctl(&s, c);
                        let r = s.apply(Command::SplitPane { origin: Origin::Client(ClientId(c)), workspace: ws.clone(), pane: p, dir: if d { Dir::Right } else { Dir::Down }, if_rev: None });
                        if r.is_ok() { prop_assert!(ctl_before, "I1: client split succeeded from a non-controller"); }
                    }
                    Op::CloseApi(pi) => {
                        let Some(p) = pick(pi) else { continue };
                        let _ = s.apply(Command::ClosePane { origin: Origin::Api, workspace: ws.clone(), pane: p, if_rev: None });
                    }
                    Op::Focus(c, pi) => {
                        let Some(p) = pick(pi) else { continue };
                        let ctl_before = is_ctl(&s, c);
                        let r = s.apply(Command::FocusPane { client: ClientId(c), pane: p });
                        if r.is_ok() { prop_assert!(ctl_before, "I1: focus succeeded from a non-controller"); }
                    }
                    Op::NewTab => {
                        let _ = s.apply(Command::NewTab { origin: Origin::Api, workspace: ws.clone() });
                    }
                    Op::SelectTab(ti) => {
                        let tabs = tab_ids(&s, &ws);
                        let t = tabs[ti % tabs.len()].clone();
                        let _ = s.apply(Command::SelectTab { origin: Origin::Api, workspace: ws.clone(), tab: t });
                    }
                    Op::CloseTab(ti) => {
                        let tabs = tab_ids(&s, &ws);
                        let t = tabs[ti % tabs.len()].clone();
                        let _ = s.apply(Command::CloseTab { origin: Origin::Api, workspace: ws.clone(), tab: t });
                    }
                }

                // --- invariants after every step ---
                prop_assert!(s.check_g1(), "G1 violated");
                prop_assert!(s.check_tree_consistency(), "tree consistency violated");

                let w = s.workspace(&ws).unwrap();
                let active = w.tab(&w.active_tab).unwrap();
                // G2 (conservation): the active tab's geometry tiles the viewport exactly.
                prop_assert_eq!(active.layout.footprint(w.viewport), w.viewport, "G2: geometry does not conserve the viewport");
                // G2 (strong): every terminal's ACTUAL size equals its derived share of
                // (viewport, layout) — catches stale/unreflowed geometry, not just conservation.
                let mut derived = Vec::new();
                active.layout.derive_sizes(w.viewport, &mut derived);
                for (tid, c, r) in &derived {
                    let t = s.terminal(tid).unwrap();
                    prop_assert_eq!((t.cols, t.rows), (*c, *r), "G2: terminal size != derived geometry");
                }

                // workspace rev monotonic
                prop_assert!(w.rev >= last_ws_rev, "workspace rev decreased");
                last_ws_rev = w.rev;

                // every terminal's rev is monotonic
                for p in panes(&s, &ws) {
                    let tabs = &s.workspace(&ws).unwrap().tabs;
                    if let Some(tid) = tabs.iter().find_map(|t| t.layout.terminal_of(&p)) {
                        let cur = s.terminal(tid).unwrap().rev;
                        let prev = term_rev.entry(tid.clone()).or_insert(0);
                        prop_assert!(cur >= *prev, "terminal rev decreased");
                        *prev = cur;
                    }
                }
            }
        }
    }
}
