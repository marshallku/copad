use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;

use crate::panel::{Panel, PanelVariant};

pub enum SplitNode {
    Leaf {
        panel: Rc<PanelVariant>,
    },
    Branch {
        paned: gtk4::Paned,
        first: Rc<RefCell<SplitNode>>,
        second: Rc<RefCell<SplitNode>>,
    },
}

impl SplitNode {
    pub fn widget(&self) -> &gtk4::Widget {
        match self {
            SplitNode::Leaf { panel } => panel.widget(),
            SplitNode::Branch { paned, .. } => paned.upcast_ref(),
        }
    }

    pub fn collect_panels(&self, out: &mut Vec<Rc<PanelVariant>>) {
        match self {
            SplitNode::Leaf { panel } => out.push(panel.clone()),
            SplitNode::Branch { first, second, .. } => {
                first.borrow().collect_panels(out);
                second.borrow().collect_panels(out);
            }
        }
    }

    /// Find the sibling panels of target (panels in the other side of the same split)
    pub fn find_sibling_panels(&self, target: &Rc<PanelVariant>) -> Vec<Rc<PanelVariant>> {
        if let Some((parent_node, side)) = Self::find_parent_of_root(self, target) {
            let borrowed = parent_node.borrow();
            if let SplitNode::Branch { first, second, .. } = &*borrowed {
                let sibling = match side {
                    ChildSide::First => second,
                    ChildSide::Second => first,
                };
                let mut out = Vec::new();
                sibling.borrow().collect_panels(&mut out);
                return out;
            }
        }
        Vec::new()
    }

    fn find_parent(
        node: &Rc<RefCell<SplitNode>>,
        target: &Rc<PanelVariant>,
    ) -> Option<(Rc<RefCell<SplitNode>>, ChildSide)> {
        let borrowed = node.borrow();
        if let SplitNode::Branch { first, second, .. } = &*borrowed {
            if let SplitNode::Leaf { panel } = &*first.borrow()
                && Rc::ptr_eq(panel, target)
            {
                return Some((node.clone(), ChildSide::First));
            }
            if let SplitNode::Leaf { panel } = &*second.borrow()
                && Rc::ptr_eq(panel, target)
            {
                return Some((node.clone(), ChildSide::Second));
            }
            if let Some(found) = Self::find_parent(first, target) {
                return Some(found);
            }
            if let Some(found) = Self::find_parent(second, target) {
                return Some(found);
            }
        }
        None
    }

    /// Same as find_parent but works on a non-Rc root (for sibling search)
    fn find_parent_of_root(
        node: &SplitNode,
        target: &Rc<PanelVariant>,
    ) -> Option<(Rc<RefCell<SplitNode>>, ChildSide)> {
        if let SplitNode::Branch { first, second, .. } = node {
            if let SplitNode::Leaf { panel } = &*first.borrow()
                && Rc::ptr_eq(panel, target)
            {
                // Can't return a ref to ourselves without Rc, search children instead
            }
            // Delegate to Rc-based search
            if let Some(found) = Self::find_parent(first, target) {
                return Some(found);
            }
            if let Some(found) = Self::find_parent(second, target) {
                return Some(found);
            }
            // Check if target is direct child of this node
            if let SplitNode::Leaf { panel } = &*first.borrow()
                && Rc::ptr_eq(panel, target)
            {
                // We need the Rc wrapper - use root-level find
            }
        }
        None
    }

    fn find_branch_parent(
        node: &Rc<RefCell<SplitNode>>,
        target: &Rc<RefCell<SplitNode>>,
    ) -> Option<(Rc<RefCell<SplitNode>>, ChildSide)> {
        let borrowed = node.borrow();
        if let SplitNode::Branch { first, second, .. } = &*borrowed {
            if Rc::ptr_eq(first, target) {
                return Some((node.clone(), ChildSide::First));
            }
            if Rc::ptr_eq(second, target) {
                return Some((node.clone(), ChildSide::Second));
            }
            if let Some(found) = Self::find_branch_parent(first, target) {
                return Some(found);
            }
            if let Some(found) = Self::find_branch_parent(second, target) {
                return Some(found);
            }
        }
        None
    }

    fn clone_shallow(&self) -> SplitNode {
        match self {
            SplitNode::Leaf { panel } => SplitNode::Leaf {
                panel: panel.clone(),
            },
            SplitNode::Branch {
                paned,
                first,
                second,
            } => SplitNode::Branch {
                paned: paned.clone(),
                first: first.clone(),
                second: second.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ChildSide {
    First,
    Second,
}

fn make_paned(orientation: gtk4::Orientation) -> gtk4::Paned {
    let paned = gtk4::Paned::new(orientation);
    paned.set_hexpand(true);
    paned.set_vexpand(true);
    paned.set_wide_handle(true);
    paned.set_shrink_start_child(false);
    paned.set_shrink_end_child(false);
    paned
}

fn set_paned_position_deferred(paned: &gtk4::Paned, orientation: gtk4::Orientation) {
    let p = paned.clone();
    glib::timeout_add_local_once(Duration::from_millis(30), move || {
        let size = match orientation {
            gtk4::Orientation::Horizontal => p.width(),
            _ => p.height(),
        };
        if size > 0 {
            p.set_position(size / 2);
        }
    });
}

/// A tab's content: stable container + split tree
pub struct TabContent {
    pub container: gtk4::Box,
    pub root: Rc<RefCell<SplitNode>>,
}

impl TabContent {
    pub fn new(panel: Rc<PanelVariant>) -> Self {
        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        container.set_hexpand(true);
        container.set_vexpand(true);
        container.append(panel.widget());

        let root = Rc::new(RefCell::new(SplitNode::Leaf { panel }));
        Self { container, root }
    }

    /// Split the focused panel. Returns the sibling panels for focus fallback.
    pub fn split(
        &self,
        focused: &Rc<PanelVariant>,
        new_panel: &Rc<PanelVariant>,
        orientation: gtk4::Orientation,
    ) {
        let paned = make_paned(orientation);
        let old_widget = focused.widget().clone();

        let is_root = {
            let root = self.root.borrow();
            matches!(&*root, SplitNode::Leaf { panel } if Rc::ptr_eq(panel, focused))
        };

        if is_root {
            self.container.remove(&old_widget);

            paned.set_start_child(Some(&old_widget));
            paned.set_end_child(Some(new_panel.widget()));

            let old_leaf = Rc::new(RefCell::new(SplitNode::Leaf {
                panel: focused.clone(),
            }));
            let new_leaf = Rc::new(RefCell::new(SplitNode::Leaf {
                panel: new_panel.clone(),
            }));

            *self.root.borrow_mut() = SplitNode::Branch {
                paned: paned.clone(),
                first: old_leaf,
                second: new_leaf,
            };

            self.container.append(paned.upcast_ref::<gtk4::Widget>());
        } else if let Some((parent_node, side)) = SplitNode::find_parent(&self.root, focused) {
            let mut parent = parent_node.borrow_mut();
            if let SplitNode::Branch {
                paned: parent_paned,
                first,
                second,
            } = &mut *parent
            {
                match side {
                    ChildSide::First => parent_paned.set_start_child(gtk4::Widget::NONE),
                    ChildSide::Second => parent_paned.set_end_child(gtk4::Widget::NONE),
                }

                paned.set_start_child(Some(&old_widget));
                paned.set_end_child(Some(new_panel.widget()));

                let old_leaf = Rc::new(RefCell::new(SplitNode::Leaf {
                    panel: focused.clone(),
                }));
                let new_leaf = Rc::new(RefCell::new(SplitNode::Leaf {
                    panel: new_panel.clone(),
                }));

                let new_branch = Rc::new(RefCell::new(SplitNode::Branch {
                    paned: paned.clone(),
                    first: old_leaf,
                    second: new_leaf,
                }));

                match side {
                    ChildSide::First => {
                        parent_paned.set_start_child(Some(paned.upcast_ref::<gtk4::Widget>()));
                        *first = new_branch;
                    }
                    ChildSide::Second => {
                        parent_paned.set_end_child(Some(paned.upcast_ref::<gtk4::Widget>()));
                        *second = new_branch;
                    }
                }
            }
        }

        set_paned_position_deferred(&paned, orientation);
    }

    /// Close a panel. Returns the sibling panel to focus, or None if the tab should close.
    pub fn close_panel(&self, target: &Rc<PanelVariant>) -> CloseResult {
        // Root is a single leaf → close the tab
        {
            let root = self.root.borrow();
            if let SplitNode::Leaf { panel } = &*root
                && Rc::ptr_eq(panel, target)
            {
                return CloseResult::CloseTab;
            }
        }

        // Collect sibling panels before modifying the tree
        let sibling_panels = {
            let root = self.root.borrow();
            root.find_sibling_panels(target)
        };

        if let Some((parent_node, side)) = SplitNode::find_parent(&self.root, target) {
            let sibling_node;
            let sibling_widget;

            {
                let parent = parent_node.borrow();
                if let SplitNode::Branch { first, second, .. } = &*parent {
                    let sibling = match side {
                        ChildSide::First => second,
                        ChildSide::Second => first,
                    };
                    sibling_widget = sibling.borrow().widget().clone();
                    sibling_node = sibling.clone();
                } else {
                    return CloseResult::CloseTab;
                }
            }

            let parent_is_root = Rc::ptr_eq(&parent_node, &self.root);

            if parent_is_root {
                let old_widget = self.root.borrow().widget().clone();
                self.container.remove(&old_widget);

                {
                    let parent = parent_node.borrow();
                    if let SplitNode::Branch { paned, .. } = &*parent {
                        paned.set_start_child(gtk4::Widget::NONE);
                        paned.set_end_child(gtk4::Widget::NONE);
                    }
                }

                let new_root = sibling_node.borrow().clone_shallow();
                self.container.append(new_root.widget());
                *self.root.borrow_mut() = new_root;
            } else if let Some((grandparent_node, parent_side)) =
                SplitNode::find_branch_parent(&self.root, &parent_node)
            {
                {
                    let parent = parent_node.borrow();
                    if let SplitNode::Branch { paned, .. } = &*parent {
                        paned.set_start_child(gtk4::Widget::NONE);
                        paned.set_end_child(gtk4::Widget::NONE);
                    }
                }

                let mut grandparent = grandparent_node.borrow_mut();
                if let SplitNode::Branch {
                    paned: gp_paned,
                    first,
                    second,
                } = &mut *grandparent
                {
                    match parent_side {
                        ChildSide::First => {
                            gp_paned.set_start_child(gtk4::Widget::NONE);
                            gp_paned.set_start_child(Some(&sibling_widget));
                            *first = sibling_node;
                        }
                        ChildSide::Second => {
                            gp_paned.set_end_child(gtk4::Widget::NONE);
                            gp_paned.set_end_child(Some(&sibling_widget));
                            *second = sibling_node;
                        }
                    }
                }
            }

            // Return the closest sibling for focus
            let focus_target = sibling_panels.into_iter().next();
            CloseResult::Closed { focus_target }
        } else {
            CloseResult::CloseTab
        }
    }
}

pub enum CloseResult {
    CloseTab,
    Closed {
        focus_target: Option<Rc<PanelVariant>>,
    },
}
